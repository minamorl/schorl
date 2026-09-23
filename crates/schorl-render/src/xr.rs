//! OpenXR の graphics binding を Vulkan で張り、swapchain へ描き込んで submit する面。
//!
//! # なぜ零コピーを買わないか
//!
//! クライアントの dmabuf をそのまま swapchain のテクスチャにできれば複製が消える。
//! しかし `xrEnumerateSwapchainImages` の逐語は swapchain 画像がランタイムの持ち物
//! であることをこう述べている:
//!
//! > Fills an array of graphics API-specific `XrSwapchainImage` structures.
//! > The resources **must** be constant and valid for the lifetime of the XrSwapchain.
//!
//! > Runtimes **must** always return identical buffer contents from this
//! > enumeration for the lifetime of the swapchain.
//!
//! 「lifetime にわたって不変で同一」と要求されている列挙の中身を、フレームごとに
//! 変わるクライアントのバッファへ差し替える経路は一次資料に見当たらない。よって
//! import した `VkImage` は sampler から読み、ランタイムが確保した画像へ描き込む。
//! これは `free schorl.compositor.swapchain_binding` の中での選択であって pin ではない。
//!
//! # 黒を自分で持つということ
//!
//! `pin space.background` を満たすために二つを同時にやる。
//!
//! 1. 環境の合成を `OPAQUE` に固定する。透過や加算では背景がランタイム側の
//!    映像になり、自分の黒が見えない。`OPAQUE` を出さないランタイムは
//!    封筒で拒む (**黙って別の blend へ落ちない**)。
//! 2. projection 層の全画素を毎フレーム黒で clear する。ランタイム既定の void に
//!    任せない。`pin space.extent = 360_degrees` に対しては、projection 層が
//!    view の錐台を丸ごと埋めるので頭がどこを向いても自分の黒が出る。

use ash::vk::{self, Handle as _};
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::frame::Frame;
use schorl_core::id::TraceId;
use schorl_core::retry::{Backoff, RetryPolicy};
use schorl_panel::panel::Panel;
use schorl_scope::Background;
use schorl_xr::{SessionConfig, XrSession};

use crate::facts::{RenderFacts, TextureRoute};
use crate::renderer::{DEFAULT_MAX_SURFACES_PER_FRAME, RenderTarget, Renderer};
use crate::space::{Surface, SurfaceSize, ViewProjection};
use crate::texture::Texture;
use crate::vulkan::{VulkanContext, openxr_failure};

/// この lane が使う view configuration。
///
/// `free` な軸だが、HMD は両眼なので stereo を採る。
pub const VIEW_CONFIGURATION: openxr::ViewConfigurationType =
    openxr::ViewConfigurationType::PRIMARY_STEREO;

/// swapchain の色形式。
///
/// ランタイムが並べた形式の中からこの順で選ぶ。`B8G8R8A8` を先に置くのは
/// dmabuf 側の `DRM_FORMAT_XRGB8888` / `ARGB8888` と byte 順が揃うためで、
/// 途中で並べ替える段を挟まずに済む。
pub const PREFERRED_COLOR_FORMATS: [vk::Format; 4] = [
    vk::Format::B8G8R8A8_SRGB,
    vk::Format::R8G8B8A8_SRGB,
    vk::Format::B8G8R8A8_UNORM,
    vk::Format::R8G8B8A8_UNORM,
];

/// 近平面と遠平面 (メートル)。`free` な軸。
const NEAR_Z: f32 = 0.05;
/// 遠平面 (メートル)。
const FAR_Z: f32 = 100.0;

/// 開いたセッションについて機械が言い切れる事実。
///
/// `pin verify.machine_scope` の `openxr_session_opens` と
/// `client_frame_reaches_swapchain` を、要約ではなく数字で持つ。
/// **`hmd_accepted` を表す欄は無い** (`verify.no_green_substitute`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XrVulkanFacts {
    /// ランタイムの名前。
    pub runtime_name: String,
    /// ランタイムの版。
    pub runtime_version: String,
    /// ランタイムが並べた拡張の数。
    pub advertised_extension_count: usize,
    /// `XR_KHR_vulkan_enable2` が並んでいたか。
    pub vulkan_enable2_advertised: bool,
    /// 使った view の数。
    pub view_count: usize,
    /// ランタイムが並べた environment blend mode の綴り。
    pub offered_blend_modes: Vec<String>,
    /// 実際に使った blend mode の綴り。
    pub chosen_blend_mode: String,
    /// ランタイムが並べた swapchain 形式の数。
    pub advertised_swapchain_format_count: usize,
    /// 実際に選んだ `VkFormat` の生の値。
    pub chosen_swapchain_format: u32,
    /// 使った参照空間の綴り。
    pub reference_space: String,
    /// セッションが辿った状態の綴り。
    pub observed_states: Vec<String>,
    /// Vulkan 側の事実。
    pub vulkan: crate::vulkan::VulkanDeviceFacts,
}

/// `XR_KHR_vulkan_enable2` でフルセッションを開く capability。
#[derive(Debug, Clone)]
pub struct XrVulkanRuntime {
    application_name: String,
    background: Background,
    max_surfaces: u32,
    ready_wait: RetryPolicy,
}

impl XrVulkanRuntime {
    /// 既定の構えで作る。
    ///
    /// `READY` を待つ待ち直しは `pin code.retry.backoff` と `pin code.retry.max`
    /// により指数 + jitter で上限 5 回である。
    pub fn new(application_name: impl Into<String>) -> Result<Self> {
        Ok(Self {
            application_name: application_name.into(),
            background: Background::Black,
            max_surfaces: DEFAULT_MAX_SURFACES_PER_FRAME,
            ready_wait: RetryPolicy::new(
                5,
                Backoff::ExponentialJitter {
                    base_millis: 20,
                    cap_millis: 400,
                },
            )?,
        })
    }

    /// 一度に描けるサーフェスの上限を変える。
    ///
    /// これは descriptor pool の確保量であって仕様の上限ではない
    /// (`pin v1.window_count = one_or_more` に上限は無い)。
    pub const fn with_max_surfaces(mut self, max_surfaces: u32) -> Self {
        self.max_surfaces = max_surfaces;
        self
    }

    /// セッションを開く。
    ///
    /// 手順は openxr crate の `examples/vulkan.rs` と同じ順序である:
    /// 拡張を数える → instance → system → blend mode → Vulkan を作らせる →
    /// `create_session` → 参照空間 → swapchain。
    pub fn open(&self) -> Result<XrVulkanSession> {
        // SAFETY: loader を実行時に開く。居なければ Err が返る。
        let entry = unsafe { openxr::Entry::load(&()) }.map_err(|e| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "the OpenXR loader could not be opened at run time",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })?;
        let advertised = entry
            .enumerate_extensions()
            .map_err(|e| openxr_failure("xrEnumerateInstanceExtensionProperties failed", e))?;
        let advertised_extension_count = count_advertised(&advertised);
        if !advertised.khr_vulkan_enable2 {
            return Err(Error::new(
                ErrorCode::CapabilityUnavailable,
                "the active OpenXR runtime does not advertise XR_KHR_vulkan_enable2",
                TraceId::unattributed(),
            )
            .with_detail(
                "advertised_extension_count",
                advertised_extension_count as i64,
            ));
        }
        let mut wanted = openxr::ExtensionSet::default();
        wanted.khr_vulkan_enable2 = true;

        let instance = entry
            .create_instance(
                &openxr::ApplicationInfo {
                    application_name: &self.application_name,
                    application_version: 1,
                    engine_name: "schorl",
                    engine_version: 1,
                    api_version: openxr::Version::new(1, 0, 0),
                },
                &wanted,
                &[],
                &(),
            )
            .map_err(|e| openxr_failure("xrCreateInstance failed", e))?;
        let properties = instance
            .properties()
            .map_err(|e| openxr_failure("xrGetInstanceProperties failed", e))?;
        let system = instance
            .system(openxr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .map_err(|e| openxr_failure("xrGetSystem failed", e))?;

        let offered = instance
            .enumerate_environment_blend_modes(system, VIEW_CONFIGURATION)
            .map_err(|e| openxr_failure("xrEnumerateEnvironmentBlendModes failed", e))?;
        let offered_blend_modes: Vec<String> = offered.iter().map(|m| format!("{m:?}")).collect();
        // 黒を自分で持つには OPAQUE でなければならない。無ければ封筒で止まる。
        // 黙って別の blend へ落ちると背景がこちらの持ち物でなくなる。
        if !offered.contains(&openxr::EnvironmentBlendMode::OPAQUE) {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "the runtime offers no OPAQUE environment blend mode, so the black background cannot be owned by schorl",
                TraceId::unattributed(),
            )
            .with_detail("offered", offered_blend_modes.join(",")));
        }
        let blend_mode = openxr::EnvironmentBlendMode::OPAQUE;

        let views = instance
            .enumerate_view_configuration_views(system, VIEW_CONFIGURATION)
            .map_err(|e| openxr_failure("xrEnumerateViewConfigurationViews failed", e))?;
        if views.is_empty() {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "the runtime enumerated no views for the primary stereo configuration",
                TraceId::unattributed(),
            ));
        }

        // SAFETY: instance はこの関数が作ったもので、以後 context より長く生きる
        // ように同じ構造体へ一緒に持つ。
        let context = unsafe { VulkanContext::through_openxr(&instance, system) }?;

        // SAFETY: context の instance / physical device / device は、たったいま
        // ランタイムに作らせた組である。
        let (session, frame_waiter, frame_stream) = unsafe {
            instance.create_session::<openxr::Vulkan>(
                system,
                &openxr::vulkan::SessionCreateInfo {
                    instance: context.instance().handle().as_raw() as _,
                    physical_device: context.physical_device().as_raw() as _,
                    device: context.device().handle().as_raw() as _,
                    queue_family_index: context.queue_family_index(),
                    queue_index: 0,
                },
            )
        }
        .map_err(|e| openxr_failure("xrCreateSession failed", e))?;

        let formats = session
            .enumerate_swapchain_formats()
            .map_err(|e| openxr_failure("xrEnumerateSwapchainFormats failed", e))?;
        let advertised_swapchain_format_count = formats.len();
        let color_format = PREFERRED_COLOR_FORMATS
            .iter()
            .copied()
            .find(|wanted| formats.iter().any(|f| *f == wanted.as_raw() as u32))
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::Unsupported,
                    "the runtime offers none of the colour formats this renderer can use",
                    TraceId::unattributed(),
                )
                .with_detail(
                    "advertised_swapchain_format_count",
                    advertised_swapchain_format_count as i64,
                )
            })?;

        // 参照空間。STAGE が無ければ LOCAL へ落ちる。どちらも世界固定であり、
        // `VIEW` は作らない (`forbid schorl.window.pose = head_locked`)。
        let spaces = session
            .enumerate_reference_spaces()
            .map_err(|e| openxr_failure("xrEnumerateReferenceSpaces failed", e))?;
        let (space_type, space_name) = if spaces.contains(&openxr::ReferenceSpaceType::STAGE) {
            (openxr::ReferenceSpaceType::STAGE, "stage")
        } else if spaces.contains(&openxr::ReferenceSpaceType::LOCAL) {
            (openxr::ReferenceSpaceType::LOCAL, "local")
        } else {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "the runtime offers neither a STAGE nor a LOCAL reference space",
                TraceId::unattributed(),
            ));
        };
        let space = session
            .create_reference_space(space_type, openxr::Posef::IDENTITY)
            .map_err(|e| openxr_failure("xrCreateReferenceSpace failed", e))?;

        let renderer = Renderer::new(&context, color_format, self.background, self.max_surfaces)?;

        let extent = vk::Extent2D {
            width: views[0].recommended_image_rect_width,
            height: views[0].recommended_image_rect_height,
        };
        let mut facts = RenderFacts::new();
        facts.swapchain_extent = Some((extent.width, extent.height));
        facts.swapchain_format = Some(color_format.as_raw() as u32);

        // view ごとに swapchain を一つ作る。array_size = 1 なので multiview を
        // 要求しない (要求が少ないほど動くホストが多い)。
        let mut swapchains = Vec::with_capacity(views.len());
        for _ in &views {
            let handle = session
                .create_swapchain(&openxr::SwapchainCreateInfo {
                    create_flags: openxr::SwapchainCreateFlags::EMPTY,
                    usage_flags: openxr::SwapchainUsageFlags::COLOR_ATTACHMENT
                        | openxr::SwapchainUsageFlags::SAMPLED
                        // ランタイムへ返す前に中身を覗けるようにする。
                        // **「submit した」と「submit した絵に自分の黒と板が
                        // 入っていた」は別の主張**なので、後者を測る口が要る。
                        | openxr::SwapchainUsageFlags::TRANSFER_SRC,
                    format: color_format.as_raw() as u32,
                    sample_count: 1,
                    width: extent.width,
                    height: extent.height,
                    face_count: 1,
                    array_size: 1,
                    mip_count: 1,
                })
                .map_err(|e| openxr_failure("xrCreateSwapchain failed", e))?;
            let images = handle
                .enumerate_images()
                .map_err(|e| openxr_failure("xrEnumerateSwapchainImages failed", e))?;
            facts.swapchains_created += 1;
            facts.swapchain_image_counts.push(images.len() as u32);
            let mut targets = Vec::with_capacity(images.len());
            for raw in images {
                let image = vk::Image::from_raw(raw);
                // SAFETY: raw はランタイムが並べた、この device の VkImage であり、
                // 形式と寸法は作成時に指定したものと一致する。
                let target = unsafe {
                    RenderTarget::new(&renderer, image, color_format, extent.width, extent.height)
                }?;
                targets.push(target);
            }
            swapchains.push(SwapchainForView { handle, targets });
        }

        let facts_head = XrVulkanFacts {
            runtime_name: properties.runtime_name.clone(),
            runtime_version: properties.runtime_version.to_string(),
            advertised_extension_count,
            vulkan_enable2_advertised: advertised.khr_vulkan_enable2,
            view_count: views.len(),
            offered_blend_modes,
            chosen_blend_mode: format!("{blend_mode:?}"),
            advertised_swapchain_format_count,
            chosen_swapchain_format: color_format.as_raw() as u32,
            reference_space: space_name.to_owned(),
            observed_states: Vec::new(),
            vulkan: context.facts().clone(),
        };

        Ok(XrVulkanSession {
            context,
            renderer,
            swapchains,
            extent,
            color_format,
            blend_mode,
            space,
            session,
            frame_waiter,
            frame_stream,
            instance,
            system,
            running: false,
            ended: false,
            facts: facts_head,
            render_facts: facts,
            ready_wait: self.ready_wait,
            last_display_time: None,
        })
    }
}

/// view 一つぶんの swapchain と、その画像へ描くための framebuffer。
struct SwapchainForView {
    handle: openxr::Swapchain<openxr::Vulkan>,
    targets: Vec<RenderTarget>,
}

/// 開いた Vulkan セッション。
///
/// **落とす順序が効く。** openxr crate の `examples/vulkan.rs` の逐語:
///
/// > OpenXR MUST be allowed to clean up before we destroy Vulkan resources it
/// > could touch, so first we must drop all its handles.
///
/// Rust は構造体の欄を**宣言順**に落とすので、openxr の handle を先に、Vulkan を
/// 後に置く。この並びを崩すと、ランタイムが掴んでいる `VkDevice` を先に壊して
/// segfault する (実測: 並びを逆にしていたとき exit 139)。
///
/// 明示的に [`XrVulkanSession::close`] を通すのが正しい道で、`Drop` はその
/// 取りこぼしを拾うだけである。
pub struct XrVulkanSession {
    // --- ここから openxr の handle。先に落ちる。---
    /// view ごとの swapchain。Session の複製を内に持つので最初に落とす。
    swapchains: Vec<SwapchainForView>,
    space: openxr::Space,
    frame_stream: openxr::FrameStream<openxr::Vulkan>,
    frame_waiter: openxr::FrameWaiter,
    session: openxr::Session<openxr::Vulkan>,
    instance: openxr::Instance,
    // --- ここから Vulkan。後に落ちる。---
    /// render pass / pipeline など。device より先に落ちること。
    renderer: Renderer,
    /// `VkDevice` と `VkInstance`。**必ず最後**。
    context: VulkanContext,
    // --- 以下は資源を持たない値。---
    extent: vk::Extent2D,
    color_format: vk::Format,
    blend_mode: openxr::EnvironmentBlendMode,
    system: openxr::SystemId,
    running: bool,
    ended: bool,
    facts: XrVulkanFacts,
    render_facts: RenderFacts,
    ready_wait: RetryPolicy,
    /// 直近の `xrWaitFrame` が返した予測表示時刻。
    ///
    /// `xrLocateSpace` / `xrSyncActions` は時刻を要る。手の姿勢を引く面は
    /// この session の外に居るので、こちらが観測した時刻を読める口を開ける。
    /// **この欄は描画の判断に使われない** — 記録するだけである。
    last_display_time: Option<openxr::Time>,
}

impl core::fmt::Debug for XrVulkanSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XrVulkanSession")
            .field("facts", &self.facts)
            .field("render_facts", &self.render_facts)
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

impl XrVulkanSession {
    /// 観測できた事実。
    pub const fn facts(&self) -> &XrVulkanFacts {
        &self.facts
    }

    /// 描いて出したことの帳面。
    pub const fn render_facts(&self) -> &RenderFacts {
        &self.render_facts
    }

    /// Vulkan の面。テクスチャを作るのに要る。
    pub const fn vulkan(&self) -> &VulkanContext {
        &self.context
    }

    /// swapchain 一枚の寸法。
    pub const fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// `system` id。
    pub const fn system(&self) -> openxr::SystemId {
        self.system
    }

    /// OpenXR の instance。
    ///
    /// action set を張る面はこの crate の外に居る (掴みは `schorl-panel` の算術で、
    /// 配線は境界の仕事である)。読み取りの借用だけを開ける。
    pub const fn openxr_instance(&self) -> &openxr::Instance {
        &self.instance
    }

    /// OpenXR の session。action set を attach する面が要る。
    pub const fn openxr_session(&self) -> &openxr::Session<openxr::Vulkan> {
        &self.session
    }

    /// 描いている参照空間。手の姿勢をこの空間で引くために要る。
    pub const fn reference_space(&self) -> &openxr::Space {
        &self.space
    }

    /// 直近の `xrWaitFrame` が返した予測表示時刻。まだ一枚も待っていなければ `None`。
    pub const fn last_display_time(&self) -> Option<openxr::Time> {
        self.last_display_time
    }

    /// 出来事を吸って状態を進める。
    ///
    /// `READY` で `xrBeginSession`、`STOPPING` で `xrEndSession`。
    /// `EXITING` / `LOSS_PENDING` を見たら真を返す (呼び手は回すのを止める)。
    pub fn pump_events(&mut self) -> Result<bool> {
        let mut exiting = false;
        // 入れ物は呼びごとに立てる。openxr::EventDataBuffer は生ポインタを含む
        // ので `Send` ではなく、`XrSession: Send` を満たすために持ち歩けない。
        let mut buffer = openxr::EventDataBuffer::new();
        while let Some(event) = self
            .instance
            .poll_event(&mut buffer)
            .map_err(|e| openxr_failure("xrPollEvent failed", e))?
        {
            if let openxr::Event::SessionStateChanged(changed) = event {
                let state = changed.state();
                self.facts.observed_states.push(format!("{state:?}"));
                match state {
                    openxr::SessionState::READY => {
                        self.session
                            .begin(VIEW_CONFIGURATION)
                            .map_err(|e| openxr_failure("xrBeginSession failed", e))?;
                        self.running = true;
                    }
                    openxr::SessionState::STOPPING => {
                        self.session
                            .end()
                            .map_err(|e| openxr_failure("xrEndSession failed", e))?;
                        self.running = false;
                    }
                    openxr::SessionState::EXITING | openxr::SessionState::LOSS_PENDING => {
                        exiting = true;
                    }
                    _ => {}
                }
            }
        }
        Ok(exiting)
    }

    /// セッションが走っているか。
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// `READY` になるまで待ち直す。
    ///
    /// 待ちは `pin code.retry.backoff` の指数 + jitter で、回数は
    /// `pin code.retry.max` の上限内。返り値はセッションが走り出したかどうか。
    pub fn wait_until_running(&mut self, sleeper: &dyn schorl_xr::Sleeper) -> Result<bool> {
        let mut attempts: u8 = 0;
        loop {
            if self.pump_events()? {
                return Ok(false);
            }
            if self.running {
                return Ok(true);
            }
            if !self.ready_wait.should_retry(attempts) {
                return Ok(false);
            }
            // jitter の種は時刻の下位桁から引く。乱数の capability をこの面に
            // 持ち込まないための最小の形。
            let jitter = jitter_unit();
            let delay = self.ready_wait.delay_millis(u32::from(attempts), jitter);
            sleeper.sleep_millis(delay);
            attempts = attempts.saturating_add(1);
        }
    }

    /// 一枚描いて出す。
    ///
    /// `surfaces` は 0 枚でもよい。0 枚でも背景の黒は毎 view 書かれる。
    /// 返るのは「この呼びで submit まで到達したか」。
    pub fn submit_frame(&mut self, surfaces: &[Surface]) -> Result<bool> {
        self.submit_frame_capturing(surfaces, false)
            .map(|(submitted, _)| submitted)
    }

    /// 一枚描いて出し、望めば view 0 の swapchain 画像を CPU へ写して返す。
    ///
    /// 写しはランタイムへ返す前に行い、layout は `COLOR_ATTACHMENT_OPTIMAL` へ
    /// 戻す。**「xrEndFrame を呼んだ」と「渡した絵に自分の黒と板が入っていた」は
    /// 別の主張**なので、後者を測れる口をここに置く。
    pub fn submit_frame_capturing(
        &mut self,
        surfaces: &[Surface],
        capture_view0: bool,
    ) -> Result<(bool, Option<Vec<u8>>)> {
        if !self.running {
            return Err(Error::new(
                ErrorCode::HostRefused,
                "the OpenXR session is not running, so no frame can be submitted",
                TraceId::unattributed(),
            ));
        }
        let mut captured: Option<Vec<u8>> = None;
        let frame_state = self
            .frame_waiter
            .wait()
            .map_err(|e| openxr_failure("xrWaitFrame failed", e))?;
        self.last_display_time = Some(frame_state.predicted_display_time);
        self.frame_stream
            .begin()
            .map_err(|e| openxr_failure("xrBeginFrame failed", e))?;

        if !frame_state.should_render {
            self.frame_stream
                .end(frame_state.predicted_display_time, self.blend_mode, &[])
                .map_err(|e| openxr_failure("xrEndFrame for a skipped frame failed", e))?;
            self.render_facts.record_skip();
            return Ok((false, None));
        }

        let (_, views) = self
            .session
            .locate_views(
                VIEW_CONFIGURATION,
                frame_state.predicted_display_time,
                &self.space,
            )
            .map_err(|e| openxr_failure("xrLocateViews failed", e))?;
        if views.len() != self.swapchains.len() {
            return Err(Error::new(
                ErrorCode::Internal,
                "the runtime located a different number of views than we made swapchains for",
                TraceId::unattributed(),
            )
            .with_detail("located", views.len() as i64)
            .with_detail("swapchains", self.swapchains.len() as i64));
        }

        // 借用の組へ写す。テクスチャの所有は呼び手のままで、描く側は借りる。
        let draws: Vec<crate::space::SurfaceDraw<'_>> =
            surfaces.iter().map(Surface::as_draw).collect();
        let mut routes: Vec<TextureRoute> = Vec::new();
        let mut acquired = Vec::with_capacity(views.len());
        for (index, swapchain) in self.swapchains.iter_mut().enumerate() {
            let image_index = swapchain
                .handle
                .acquire_image()
                .map_err(|e| openxr_failure("xrAcquireSwapchainImage failed", e))?;
            swapchain
                .handle
                .wait_image(openxr::Duration::INFINITE)
                .map_err(|e| openxr_failure("xrWaitSwapchainImage failed", e))?;
            let target = swapchain.targets.get(image_index as usize).ok_or_else(|| {
                Error::new(
                    ErrorCode::Internal,
                    "the runtime acquired a swapchain image index we have no framebuffer for",
                    TraceId::unattributed(),
                )
                .with_detail("image_index", i64::from(image_index))
            })?;
            let view_projection = ViewProjection::from_openxr(views[index], NEAR_Z, FAR_Z)?;
            let drawn = self
                .renderer
                .draw_view(target, self.extent, view_projection, &draws)?;
            if index == 0 {
                routes = drawn;
                if capture_view0 {
                    // ランタイムへ返す前に覗き、返す layout へ戻す。
                    captured = Some(crate::texture::copy_image_to_host(
                        &self.context,
                        target.image(),
                        self.color_format,
                        self.extent.width,
                        self.extent.height,
                        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                        Some(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL),
                    )?);
                }
            }
            swapchain
                .handle
                .release_image()
                .map_err(|e| openxr_failure("xrReleaseSwapchainImage failed", e))?;
            acquired.push(image_index);
        }

        let rect = openxr::Rect2Di {
            offset: openxr::Offset2Di { x: 0, y: 0 },
            extent: openxr::Extent2Di {
                width: self.extent.width as i32,
                height: self.extent.height as i32,
            },
        };
        let projection_views: Vec<openxr::CompositionLayerProjectionView<'_, openxr::Vulkan>> =
            views
                .iter()
                .zip(self.swapchains.iter())
                .map(|(view, swapchain)| {
                    openxr::CompositionLayerProjectionView::new()
                        .pose(view.pose)
                        .fov(view.fov)
                        .sub_image(
                            openxr::SwapchainSubImage::new()
                                .swapchain(&swapchain.handle)
                                .image_array_index(0)
                                .image_rect(rect),
                        )
                })
                .collect();
        let layer = openxr::CompositionLayerProjection::new()
            .space(&self.space)
            .views(&projection_views);
        self.frame_stream
            .end(
                frame_state.predicted_display_time,
                self.blend_mode,
                &[&layer],
            )
            .map_err(|e| openxr_failure("xrEndFrame failed", e))?;
        self.render_facts.record_submit(&routes, views.len());
        Ok((true, captured))
    }

    /// 明示的に閉じる。
    ///
    /// `house.resource_lifecycle.release_paths` の「失敗を報告できる道」である。
    /// 先に OpenXR へ終了を伝え、device が空くまで待ってから framebuffer を返す。
    pub fn close(&mut self) -> Result<()> {
        if self.ended {
            return Ok(());
        }
        self.ended = true;
        if self.running {
            match self.session.request_exit() {
                Ok(()) => {}
                Err(openxr::sys::Result::ERROR_SESSION_NOT_RUNNING) => {}
                Err(e) => return Err(openxr_failure("xrRequestExitSession failed", e)),
            }
            // EXITING まで少し回す。来なくても device を空けて閉じる。
            for _ in 0..64 {
                if self.pump_events()? {
                    break;
                }
                if !self.running {
                    break;
                }
            }
            if self.running {
                match self.session.end() {
                    Ok(_) => {}
                    Err(openxr::sys::Result::ERROR_SESSION_NOT_STOPPING) => {}
                    Err(e) => return Err(openxr_failure("xrEndSession failed", e)),
                }
                self.running = false;
            }
        }
        self.context.wait_idle()?;
        let swapchains = core::mem::take(&mut self.swapchains);
        for swapchain in swapchains {
            // SAFETY: device は上の wait_idle で空になっている。
            unsafe { self.renderer.destroy_targets(swapchain.targets) };
            drop(swapchain.handle);
        }
        Ok(())
    }
}

impl Drop for XrVulkanSession {
    fn drop(&mut self) {
        // 明示的に閉じていなければここで拾う。失敗は Drop からは報告できないので
        // 黙って進む (報告経路は close)。
        let _ = self.close();
    }
}

/// `schorl_xr::XrSession` としての面。
///
/// あちらの `submit_frame` は CPU の [`Frame`] を受ける署名なので、**shm の退路が
/// そのまま trait の道になる**。dmabuf の経路は [`XrVulkanSession::submit_frame`]
/// を直に呼ぶ (テクスチャの持ち主が呼び手側にあるため)。
impl XrSession for XrVulkanSession {
    fn poll_events(&mut self) -> Result<Vec<schorl_xr::XrEvent>> {
        let exiting = self.pump_events()?;
        let state = if exiting {
            schorl_xr::SessionState::Stopping
        } else if self.running {
            schorl_xr::SessionState::Running
        } else {
            schorl_xr::SessionState::Idle
        };
        Ok(vec![schorl_xr::XrEvent::StateChanged(state)])
    }

    fn submit_frame(&mut self, frame: &Frame, panel: &Panel) -> Result<()> {
        let texture = Texture::upload_frame(&self.context, frame)?;
        let size = SurfaceSize::new(panel.size().width_m, panel.size().height_m)?;
        let pose = panel.pose().pose;
        let surfaces = [Surface::new(pose, size, texture)];
        self.submit_frame(&surfaces).map(|_| ())
    }

    fn end(&mut self) -> Result<()> {
        self.close()
    }

    fn presents_frames(&self) -> bool {
        true
    }
}

impl schorl_xr::XrRuntime for XrVulkanRuntime {
    fn open_session(&self, config: &SessionConfig) -> Result<Box<dyn XrSession>> {
        if config.background != self.background {
            return Err(Error::new(
                ErrorCode::Internal,
                "the session config and the renderer disagree on the background colour",
                TraceId::unattributed(),
            ));
        }
        let session = self.open()?;
        Ok(Box::new(session))
    }
}

/// jitter の種 (0.0..1.0)。
///
/// 乱数の capability をこの面へ持ち込まないために、単調時計の下位桁を使う。
/// 待ちをばらすだけの用なので分布の質は問わない。
fn jitter_unit() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1_000_000) / 1_000_000.0
}

/// `ExtensionSet` に立っている旗を数える。
///
/// 綴りを一本ずつ見ずに数だけ取る。報告に「何本並んでいたか」を出すため。
fn count_advertised(set: &openxr::ExtensionSet) -> usize {
    // Debug の綴りに現れる `: true` の数を数える。ExtensionSet の欄は版で増える
    // ので、欄を列挙するとこちらが追随を強いられる。
    format!("{set:?}").matches("true").count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preferred_formats_put_bgra_first() {
        // dmabuf 側の DRM_FORMAT_XRGB8888 / ARGB8888 と byte 順が揃う形を先に置く。
        assert_eq!(PREFERRED_COLOR_FORMATS[0], vk::Format::B8G8R8A8_SRGB);
        assert!(PREFERRED_COLOR_FORMATS.contains(&vk::Format::B8G8R8A8_UNORM));
    }

    #[test]
    fn the_view_configuration_is_stereo() {
        assert_eq!(
            VIEW_CONFIGURATION,
            openxr::ViewConfigurationType::PRIMARY_STEREO
        );
    }

    #[test]
    fn the_runtime_only_carries_black() {
        // pin space.background。背景を引数に取る口が無いこと。
        let runtime = XrVulkanRuntime::new("schorl-render-test").expect("valid retry policy");
        assert_eq!(runtime.background, Background::Black);
    }

    #[test]
    fn the_surface_cap_is_a_pool_size_not_a_spec_limit() {
        // pin v1.window_count = one_or_more。上限を差し替えられること。
        let runtime = XrVulkanRuntime::new("schorl-render-test")
            .expect("valid retry policy")
            .with_max_surfaces(97);
        assert_eq!(runtime.max_surfaces, 97);
    }

    #[test]
    fn the_ready_wait_stays_inside_the_pinned_retry_range() {
        // pin code.retry.max: range retry.max = 0..5
        let runtime = XrVulkanRuntime::new("schorl-render-test").expect("valid retry policy");
        assert!(runtime.ready_wait.max_retries() <= schorl_core::retry::MAX_RETRIES_LIMIT);
        assert!(matches!(
            runtime.ready_wait.backoff(),
            Backoff::ExponentialJitter { .. }
        ));
    }

    #[test]
    fn the_jitter_seed_stays_in_the_unit_interval() {
        for _ in 0..64 {
            let unit = jitter_unit();
            assert!((0.0..1.0).contains(&unit), "got {unit}");
        }
    }

    #[test]
    fn opening_a_session_without_a_runtime_returns_an_envelope_not_a_panic() {
        // ランタイムが居ないホストでも build は通り、不在は封筒で返る。
        // ここが居る機では成功しうるので、どちらでも panic しないことだけを測る。
        let runtime = XrVulkanRuntime::new("schorl-render-test").expect("valid retry policy");
        match runtime.open() {
            Ok(mut session) => {
                let _ = session.close();
            }
            Err(e) => {
                assert!(
                    matches!(
                        e.code(),
                        ErrorCode::CapabilityUnavailable
                            | ErrorCode::HostRefused
                            | ErrorCode::Unsupported
                    ),
                    "unexpected code {:?}",
                    e.code()
                );
            }
        }
    }
}
