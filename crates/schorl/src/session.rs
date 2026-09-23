//! compositor 面と描画面を一本の輪に繋ぐ面。
//!
//! この module が在るまで、schorl は二つの別々に測れる物だった。
//!
//! - `schorl-compositor` — 実在のクライアントが自前のソケットへ繋ぎ、
//!   `xdg_toplevel` を出し、バッファを引き渡すところまで。
//! - `schorl-render` — 黒い空間へ矩形を置き、OpenXR の swapchain へ submit する
//!   ところまで。ただし貼っていたのは**自分で作った絵**である。
//!
//! `pin verify.machine_scope` の `client_frame_reaches_swapchain` は、その二つが
//! 繋がっていることを要求している。繋ぎ目はここで、
//! [`SchorlSession::step`] の一巡がそのまま「クライアントの画素が swapchain へ
//! 乗る」一本の道である。
//!
//! # 落ちる順序
//!
//! 欄の宣言順がそのまま落ちる順である。OpenXR の子 ([`HandInput`]) が先、
//! テクスチャ ([`Stage`]) が次、セッションと `VkDevice` ([`XrVulkanSession`]) が
//! 最後。`Wayland` 側はそのあとで構わない
//! (`house.resource_lifecycle.same_scope` / `release_paths`)。

use std::sync::Arc;
use std::time::{Duration, Instant};

use schorl_compositor::driver::HeadlessSession;
use schorl_compositor::journal::{Journal, Uuidv7IdGen};
use schorl_compositor::state::CompositorSetup;
use schorl_compositor::window::{RingPlacement, WindowId};
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_core::log::LogSink;
use schorl_core::time::{Clock, SystemClock};
use schorl_input::{ButtonState, PointerButton};
use schorl_panel::cursor::PointerHold;
use schorl_render::xr::{XrVulkanRuntime, XrVulkanSession};
use schorl_xr::{PressState, XrEvent};

use crate::grab::{GrabEffect, WindowGrabs, WindowPlacement, window_under};
use crate::hands::HandInput;
use crate::pixels::{PixelInbox, relay};
use crate::stage::{Stage, StageReport};

/// 一本に繋いだ schorl。
pub struct SchorlSession {
    // --- OpenXR の子。最初に落ちる。---
    hands: Option<HandInput>,
    // --- テクスチャ。`VkDevice` より先に落ちること。---
    stage: Stage,
    // --- セッションと `VkDevice`。---
    xr: XrVulkanSession,
    // --- Wayland 側。---
    compositor: HeadlessSession,
    inbox: PixelInbox,
    grabs: WindowGrabs,
    journal: Journal,
    started: Instant,
    frames_submitted: u64,
}

impl core::fmt::Debug for SchorlSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SchorlSession")
            .field("socket", &self.compositor.socket_name())
            .field("windows_on_stage", &self.stage.len())
            .field("frames_submitted", &self.frames_submitted)
            .finish_non_exhaustive()
    }
}

/// 一巡で起きたこと。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepOutcome {
    /// この巡りで submit まで行ったか。
    pub submitted: bool,
    /// 望んだときだけ入る、view 0 の swapchain 画像の写し。
    pub captured: Option<Vec<u8>>,
    /// 並びの組み替えで分かったこと。
    pub stage: StageReport,
    /// この巡りで食べた出来事の数。
    pub events: usize,
    /// この巡りで掴みが起こしたこと。
    pub grabs: Vec<GrabEffect>,
    /// ランタイムが終了を告げたか。
    pub exiting: bool,
}

/// 組み立ての材料。
///
/// `free` な軸 (置き方・ウィンドウの既定寸法・一度に描ける枚数) はここで埋める。
/// **どれも pin ではない。**
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// OpenXR に名乗る名前。
    pub application_name: String,
    /// ウィンドウの置き方。
    pub placement: RingPlacement,
    /// toplevel へ最初に送る大きさ (画素)。
    pub default_window_size: (i32, i32),
    /// 一度に描ける矩形の上限 (descriptor pool の確保量)。
    pub max_surfaces: u32,
    /// 掴みの入口 (action set) を張るか。
    ///
    /// 張れないランタイムでも絵は出るので、失敗を封筒にして畳むかどうかを
    /// ここで選ぶ。既定は張る。
    pub wire_hands: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            application_name: "schorl".to_owned(),
            placement: RingPlacement::DEFAULT,
            default_window_size: (1280, 720),
            max_surfaces: 16,
            wire_hands: true,
        }
    }
}

impl SchorlSession {
    /// compositor を起こし、OpenXR のセッションを開き、両者を繋ぐ。
    pub fn open(options: SessionOptions, log: Arc<dyn LogSink>) -> Result<Self> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let ids = Arc::new(Uuidv7IdGen);
        let journal = Journal::with_new_trace(Arc::clone(&clock), Arc::clone(&log), ids.as_ref())?;

        let (pixel_relay, inbox) = relay(Arc::clone(&clock));
        let mut setup = CompositorSetup::new(ids, journal.clone());
        setup.importer = Box::new(pixel_relay);
        setup.placement = options.placement;
        setup.default_window_size = options.default_window_size;

        let compositor = HeadlessSession::start(setup)?;
        let _ = journal.info(format!(
            "schorl's wayland socket is {}",
            compositor.socket_name()
        ));

        let runtime =
            XrVulkanRuntime::new(options.application_name)?.with_max_surfaces(options.max_surfaces);
        let xr = runtime.open()?;
        let _ = journal.info(format!(
            "openxr session opened on {} ({} views, {} blend)",
            xr.facts().runtime_name,
            xr.facts().view_count,
            xr.facts().chosen_blend_mode
        ));

        let hands = if options.wire_hands {
            match HandInput::wire(&xr) {
                Ok(input) => {
                    let _ = journal.info(format!(
                        "hand input is wired to {:?}",
                        input.bound_profiles()
                    ));
                    Some(input)
                }
                Err(e) => {
                    // 掴めないだけで、絵は出る。**「掴めた」と数えないために
                    // 記録は残す。**
                    let _ = journal.note_failure("wiring hand input", &e);
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            hands,
            stage: Stage::new(),
            xr,
            compositor,
            inbox,
            grabs: WindowGrabs::new(),
            journal,
            started: Instant::now(),
            frames_submitted: 0,
        })
    }

    /// schorl が取った Wayland のソケット名。
    pub fn socket_name(&self) -> &str {
        self.compositor.socket_name()
    }

    /// ログの束。
    pub const fn journal(&self) -> &Journal {
        &self.journal
    }

    /// OpenXR 側のセッション。事実を読むために開けてある。
    pub const fn xr(&self) -> &XrVulkanSession {
        &self.xr
    }

    /// 掴みの入口。張れていなければ `None`。
    pub const fn hands(&self) -> Option<&HandInput> {
        self.hands.as_ref()
    }

    /// いま空間に立っている矩形。
    pub const fn stage(&self) -> &Stage {
        &self.stage
    }

    /// クライアントの画素の受け箱。
    pub const fn pixels(&self) -> &PixelInbox {
        &self.inbox
    }

    /// 追跡しているウィンドウを、掴みと描画が見る形で並べる。
    pub fn placements(&self) -> Vec<WindowPlacement> {
        self.compositor
            .state()
            .windows()
            .iter()
            .filter(|w| w.is_mapped())
            .map(|w| WindowPlacement {
                id: w.id().clone(),
                plane: w.plane(),
            })
            .collect()
    }

    /// これまでに submit した枚数。
    pub const fn frames_submitted(&self) -> u64 {
        self.frames_submitted
    }

    /// クライアントを一本、schorl のソケットへ向けて起こす。
    ///
    /// 自分の環境は書き換えず、子にだけ見せる (`wm.host_compositor_coexistence`)。
    pub fn spawn_client(&self, argv: &[String]) -> Result<Option<std::process::Child>> {
        let Some((program, args)) = argv.split_first() else {
            return Ok(None);
        };
        let child = std::process::Command::new(program)
            .args(args)
            .env("WAYLAND_DISPLAY", self.compositor.socket_name())
            .env("XDG_SESSION_TYPE", "wayland")
            .env_remove("DISPLAY")
            .spawn()
            .map_err(|e| {
                Error::new(
                    ErrorCode::HostRefused,
                    "could not start the client",
                    TraceId::unattributed(),
                )
                .with_detail("program", program.as_str())
                .caused_by(e)
            })?;
        Ok(Some(child))
    }

    /// 一巡。
    ///
    /// 1. Wayland を回す (繋ぎに来たものを入れ、commit を捌く)。
    /// 2. ランタイムの出来事を吸う。
    /// 3. 手の姿勢と掴みボタンを引き、掴みを台帳へ書き戻す。
    /// 4. 台帳と受け箱から矩形の並びを組み直す。
    /// 5. 一枚描いて出す。
    pub fn step(&mut self, capture_view0: bool) -> Result<StepOutcome> {
        let mut outcome = StepOutcome::default();
        let elapsed = self.started.elapsed();
        let time_ms = u32::try_from(elapsed.as_millis() % u128::from(u32::MAX)).unwrap_or(0);

        self.compositor.turn(time_ms)?;

        outcome.exiting = self.xr.pump_events()?;

        let events = match self.hands.as_mut() {
            Some(hands) => hands.poll(&self.xr)?,
            None => Vec::new(),
        };
        outcome.events = events.len();
        for event in &events {
            self.apply(event, time_ms, &mut outcome)?;
        }

        let placements = self.placements();
        // 消えたウィンドウを掴んだままにしない。
        if let Some(held) = self.grabs.held_window().cloned() {
            if !placements.iter().any(|p| p.id == held) {
                self.grabs.forget(&held);
            }
        }
        outcome.stage = self
            .stage
            .sync(&placements, &self.inbox, self.xr.vulkan())?;

        if self.xr.is_running() {
            let (submitted, captured) = self
                .xr
                .submit_frame_capturing(self.stage.surfaces(), capture_view0)?;
            outcome.submitted = submitted;
            outcome.captured = captured;
            if submitted {
                self.frames_submitted += 1;
            }
        }
        Ok(outcome)
    }

    /// 出来事を一つ食べる。掴みと、ウィンドウ上のカーソルと押下を配る。
    ///
    /// 掴みの算術は `schorl-panel`、配りは `schorl-compositor` が持つ。
    /// ここは呼び分けるだけで、どちらの計算もやり直さない。
    pub fn apply(
        &mut self,
        event: &XrEvent,
        time_ms: u32,
        outcome: &mut StepOutcome,
    ) -> Result<()> {
        let placements = self.placements();
        if let Some(effect) = self.grabs.on_event(event, &placements) {
            if let GrabEffect::Moved { window, pose } = &effect {
                if let Some(tracked) = self.compositor.state_mut().windows_mut().get_mut(window) {
                    tracked.set_plane_pose(*pose);
                }
            }
            outcome.grabs.push(effect);
        }

        match *event {
            XrEvent::ControllerPose { controller, pose } => {
                // 掴んでいる手はウィンドウを動かしている最中なので、カーソルは
                // 配らない。掴みと指し示しを同じ手で同時にやらせない。
                if self.grabs.held_by(controller) {
                    return Ok(());
                }
                if let Some(target) = window_under(&placements, pose.position) {
                    let window = target.id.clone();
                    self.compositor.state_mut().aim_at(
                        &window,
                        pose.position,
                        PointerHold::Up,
                        time_ms,
                    )?;
                }
            }
            XrEvent::PointerButton { state, .. } => {
                let pressed = match state {
                    PressState::Pressed => ButtonState::Pressed,
                    PressState::Released => ButtonState::Released,
                };
                self.compositor
                    .state_mut()
                    .press_button(PointerButton::Left, pressed, time_ms)?;
            }
            XrEvent::Key { keycode, state } => {
                let pressed = match state {
                    PressState::Pressed => ButtonState::Pressed,
                    PressState::Released => ButtonState::Released,
                };
                self.compositor
                    .state_mut()
                    .press_key(keycode, pressed, time_ms)?;
            }
            XrEvent::GrabButton { .. } | XrEvent::StateChanged(_) => {}
        }
        Ok(())
    }

    /// そのウィンドウがいま立っている姿勢。
    pub fn window_pose(&self, window: &WindowId) -> Option<schorl_panel::panel::PanelPose> {
        self.compositor
            .state()
            .windows()
            .get(window)
            .map(|w| w.plane().pose())
    }

    /// `READY` になるまで待ち直す。
    pub fn wait_until_running(&mut self, sleeper: &dyn schorl_xr::Sleeper) -> Result<bool> {
        self.xr.wait_until_running(sleeper)
    }

    /// 条件が満たされるまで、最長 `limit` だけ回す。
    pub fn pump_until(
        &mut self,
        limit: Duration,
        tick: Duration,
        ready: impl Fn(&Self) -> bool,
    ) -> Result<bool> {
        let deadline = Instant::now() + limit;
        loop {
            self.step(false)?;
            if ready(self) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(tick);
        }
    }

    /// 明示的に閉じる。
    ///
    /// device が空になってからテクスチャを返し、そのあと OpenXR を閉じる。
    /// `Drop` でも落ちるが、こちらは失敗を報告できる (`release_paths`)。
    pub fn close(&mut self) -> Result<()> {
        self.hands = None;
        self.xr.vulkan().wait_idle()?;
        self.stage.release();
        self.xr.close()
    }
}
