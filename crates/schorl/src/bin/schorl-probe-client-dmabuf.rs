//! schorl のソケットへ繋いで、**自分で決めた**四分割の絵を一枚
//! `zwp_linux_dmabuf_v1` で出すだけの Wayland クライアント。
//!
//! # `schorl-probe-client` との違いは経路だけである
//!
//! 申告の作法は同じ。起動のたびに 7 色から 4 色を選んで並べ、その並びを
//! **描く前に** 標準出力へ一行で申告する:
//!
//! ```text
//! schorl-probe-client-dmabuf pattern=RGBW
//! ```
//!
//! 違うのは画素の運び方で、こちらは共有メモリを一切使わない:
//!
//! - GPU 上に `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` の `VkImage` を起こし、
//!   そこへ模様を書き込む。
//! - `vkGetMemoryFdKHR` で dmabuf の fd として取り出し、
//!   `zwp_linux_buffer_params_v1` へ渡して `wl_buffer` にしてもらう。
//! - **`wl_shm` を bind しない。** 退路が無いので、絵が出たならそれは dmabuf を
//!   通った絵である。落ちるときは落ちる。
//!
//! 配置は `DRM_FORMAT_MOD_LINEAR` に絞る。compositor が申告する modifier の中から
//! 選ばなければ出した dmabuf は相手の受け口に合わないので、申告された一覧を
//! `zwp_linux_dmabuf_v1.modifier` 事象から拾って照合してから作る。
//!
//! **これは受け入れではない** (`pin verify.no_green_substitute`)。

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use schorl::probe::{QuadPattern, choose_pattern, paint_quadrants};
use schorl_core::frame::{Frame, FrameOrigin, PixelFormat};
use schorl_core::time::{Clock as _, SystemClock};
use schorl_render::dmabuf::{DrmFormat, DrmModifier, ExportableImage};
use schorl_render::vulkan::VulkanContext;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// 絵の横幅 (画素)。toplevel へ送られる既定と同じ。
const WIDTH: i32 = 1280;
/// 絵の高さ (画素)。
const HEIGHT: i32 = 720;

/// compositor が申告した (fourcc, modifier) の組と、buffer の出来。
#[derive(Debug, Default)]
struct Advertised {
    modifiers: BTreeSet<(u32, u64)>,
    created: Option<wl_buffer::WlBuffer>,
    failed: bool,
}

struct Client {
    advertised: Arc<Mutex<Advertised>>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    committed: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // version 3 の global は bind の直後に modifier 事象で一覧を流す。
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            let modifier = (u64::from(modifier_hi) << 32) | u64::from(modifier_lo);
            if let Ok(mut advertised) = state.advertised.lock() {
                advertised.modifiers.insert((format, modifier));
            }
        }
    }
}

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Ok(mut advertised) = state.advertised.lock() else {
            return;
        };
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                advertised.created = Some(buffer);
            }
            zwp_linux_buffer_params_v1::Event::Failed => advertised.failed = true,
            _ => {}
        }
    }

    // `created` は新しい `wl_buffer` を connection 上に生む。どの user data を
    // 付けるかは client 側が決めるので、その指定が要る。
    wayland_client::event_created_child!(Client, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ()),
    ]);
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let (false, Some(wl), Some(buffer)) =
                (state.committed, &state.surface, &state.buffer)
            {
                wl.attach(Some(buffer), 0, 0);
                wl.damage(0, 0, WIDTH, HEIGHT);
                wl.commit();
                state.committed = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Client {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("schorl-probe-client-dmabuf failed: {reason}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    // 並びは引数で指定できる (再現のため)。無ければ自分で選ぶ。
    let pattern = match std::env::args().nth(1) {
        Some(letters) => QuadPattern::from_letters(&letters)
            .ok_or_else(|| format!("{letters:?} is not four distinct colour letters"))?,
        None => choose_pattern(),
    };
    // **描く前に申告する。** あとから言い換えられないことがこの一行の意味である。
    println!(
        "schorl-probe-client-dmabuf pattern={}",
        pattern.to_letters()
    );
    std::io::stdout()
        .flush()
        .map_err(|e| format!("flush: {e}"))?;

    let display =
        std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY is not set".to_owned())?;
    let runtime =
        std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set".to_owned())?;
    let socket = std::path::Path::new(&runtime).join(&display);
    let stream = UnixStream::connect(&socket).map_err(|e| format!("connect {socket:?}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("from_socket: {e}"))?;

    let (globals, mut queue) =
        registry_queue_init::<Client>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();

    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .map_err(|e| format!("wl_compositor: {e}"))?;
    // **`wl_shm` は bind しない。** 退路が無いことがこのバイナリの要点である。
    let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 = globals
        .bind(&qh, 3..=3, ())
        .map_err(|e| format!("zwp_linux_dmabuf_v1: {e}"))?;
    let wm_base: xdg_wm_base::XdgWmBase = globals
        .bind(&qh, 1..=3, ())
        .map_err(|e| format!("xdg_wm_base: {e}"))?;

    let advertised = Arc::new(Mutex::new(Advertised::default()));
    let mut state = Client {
        advertised: Arc::clone(&advertised),
        surface: None,
        buffer: None,
        committed: false,
    };
    // bind の直後に流れてくる modifier 事象を受け取る。
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("roundtrip for the modifier list: {e}"))?;

    let format = DrmFormat::XRGB8888;
    let offered: Vec<DrmModifier> = advertised
        .lock()
        .map_err(|_| "the advertised list is poisoned".to_owned())?
        .modifiers
        .iter()
        .filter(|(fourcc, _)| *fourcc == format.as_u32())
        .map(|(_, modifier)| DrmModifier::from_u64(*modifier))
        .collect();
    if !offered.contains(&DrmModifier::LINEAR) {
        return Err(format!(
            "schorl does not advertise DRM_FORMAT_MOD_LINEAR for {}; offered {:?}",
            format.as_fourcc_string(),
            offered.iter().map(|m| m.as_u64()).collect::<Vec<_>>()
        ));
    }

    // --- GPU 側で絵を作って dmabuf として取り出す ---------------------------
    let context =
        VulkanContext::standalone("schorl-probe-client-dmabuf").map_err(|e| envelope(&e))?;
    if !context.supports_dmabuf_import() {
        return Err("this Vulkan device cannot export a dma_buf".to_owned());
    }
    let pixels = paint_quadrants(pattern, WIDTH as u32, HEIGHT as u32);
    let frame = Frame::new(
        // クライアントが自分で描いた絵であってホストから取った絵ではない。
        FrameOrigin::TestDouble,
        PixelFormat::Xrgb8888,
        WIDTH as u32,
        HEIGHT as u32,
        (WIDTH * 4) as u32,
        SystemClock.now_utc(),
        pixels,
    )
    .map_err(|e| envelope(&e))?;

    // `Drop` が image と memory を返す。以降のどの早期 return でも漏れない
    // (`house.resource_lifecycle.release_paths`)。
    let exportable = ExportableImage::create(
        &context,
        WIDTH as u32,
        HEIGHT as u32,
        format,
        // compositor が申告した配置の中から選ぶ。
        &[DrmModifier::LINEAR],
    )
    .map_err(|e| envelope(&e))?;
    exportable
        .fill(&context, &frame)
        .map_err(|e| envelope(&e))?;
    // 提出する前に GPU の書き込みを終わらせる。**先に渡して後から描かない。**
    context.wait_idle().map_err(|e| envelope(&e))?;
    let exported = exportable.export(&context).map_err(|e| envelope(&e))?;
    if exported.chosen_modifier != DrmModifier::LINEAR {
        return Err(format!(
            "the driver laid the image out as modifier {} instead of LINEAR",
            exported.chosen_modifier.as_u64()
        ));
    }
    eprintln!(
        "schorl-probe-client-dmabuf exported {} plane(s), modifier {}, stride {}",
        exported.descriptor.planes.len(),
        exported.chosen_modifier.as_u64(),
        exported.descriptor.planes[0].stride()
    );

    // --- protocol へ渡す ----------------------------------------------------
    let params = dmabuf.create_params(&qh, ());
    for (index, plane) in exported.descriptor.planes.iter().enumerate() {
        let modifier = exported.chosen_modifier.as_u64();
        params.add(
            plane.as_fd(),
            index as u32,
            u32::try_from(plane.offset()).map_err(|_| "plane offset overflows u32".to_owned())?,
            u32::try_from(plane.stride()).map_err(|_| "plane stride overflows u32".to_owned())?,
            (modifier >> 32) as u32,
            (modifier & 0xffff_ffff) as u32,
        );
    }
    params.create(
        WIDTH,
        HEIGHT,
        format.as_u32(),
        zwp_linux_buffer_params_v1::Flags::empty(),
    );

    // `created` か `failed` が返るまで回す。
    let deadline = Instant::now() + Duration::from_secs(10);
    let buffer = loop {
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("roundtrip while waiting for the buffer: {e}"))?;
        {
            let got = advertised
                .lock()
                .map_err(|_| "the advertised list is poisoned".to_owned())?;
            if got.failed {
                return Err("schorl refused the dma_buf (`failed`)".to_owned());
            }
            if let Some(buffer) = got.created.clone() {
                break buffer;
            }
        }
        if Instant::now() >= deadline {
            return Err("schorl never answered `created` for the dma_buf".to_owned());
        }
        std::thread::sleep(Duration::from_millis(3));
    };
    params.destroy();

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title(format!("schorl dmabuf probe {}", pattern.to_letters()));
    toplevel.set_app_id("schorl-probe-client-dmabuf".to_owned());
    surface.commit();

    state.surface = Some(surface.clone());
    state.buffer = Some(buffer);

    // 親が止めるまで回す。絵は一枚きりで、動かさない。
    let deadline = Instant::now()
        + std::env::var("SCHORL_PROBE_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(Duration::from_secs(120), Duration::from_secs);
    while Instant::now() < deadline {
        if queue.roundtrip(&mut state).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = xdg;
    let _ = toplevel;
    // image と memory はここで返る。dmabuf の fd は compositor 側が複製して
    // 持っているので、こちらが閉じても向こうの画素は生きている。
    drop(exportable);
    Ok(())
}

/// 封筒を一行に畳む。このバイナリの失敗は親の標準エラーへ出る。
fn envelope(error: &schorl_core::error::Error) -> String {
    error.envelope().to_json().render()
}
