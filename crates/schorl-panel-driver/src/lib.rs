//! `schorl-panel-driver` — 板一枚を回す driver。**v1 の経路には無い。**
//!
//! spec 0.2 の原文3「360度あるならそれ用にウィンドウマネージャー作るだけでいいのでは？」で、
//! 「他 compositor の画面を capture して板に貼り、決まった 2D 位置をホストへ注入し返す」
//! というこの loop は v1 から外れた。schorl 自身が compositor になりクライアントを
//! 直接持つなら、取り口も注入先も要らない。
//!
//! 消していないのは、[`schorl_capture`] と [`schorl_display`] を消していないのと
//! 同じ理由 — 実測資産であり、既存 Hyprland の窓を VR から見る将来の口だから。
//! `schorl-xr` から出したのは、v1 のバイナリがこの loop 経由で capture / display へ
//! つながってしまわないようにするため。中身は `schorl-xr::driver` から一行も
//! 書き換えずに運んであり、`use crate::` が `use schorl_xr::` になっただけ。
//! workspace には残るので `cargo build --workspace` と `cargo test --workspace` は
//! 引き続きここを通る。
//!
//! ここが「被っている側」と「Linux 側」の間の変換を全部持つ。持っているのは
//! 変換と状態だけで、周囲効果は注入された capability の向こう側にある
//! (`house.effect_boundary.location`)。
//!
//! 満たす pin:
//! - `v1.panel_grab: require schorl.v1.panel.reposition = grabbed_by_controller` —
//!   板を動かす入口は [`XrEvent::GrabButton`] と [`XrEvent::ControllerPose`] だけ。
//!   頭の姿勢を受け取る口がこの型に無い。
//! - `ux.panel_not_head_locked` — 置き直しは
//!   [`schorl_panel::grab::panel_pose_while_held`] を通り、返るのは世界座標の姿勢だけ。
//! - `ux.cursor_mapping: require schorl.cursor.mapping = panel_plane_projection` —
//!   板上の位置は [`schorl_panel::cursor::resolve_cursor`] だけから来る。レイと板の
//!   交点を計算する行はこの crate に一行も無い。
//! - `ux.cursor_beyond_edge` — 押している間は縁の外でも位置が出るので、そのまま
//!   Linux へ運ばれる。
//! - `v1.cursor_moves` / `v1.click_delivered` / `v1.key_delivered` — それぞれ
//!   [`schorl_input::PointerSink`] と [`schorl_input::KeyboardSink`] の公開署名を呼ぶ。
//!   **中身は別 phase が埋める。ここは呼ぶ側だけ。**
//! - `v1.panel_source` — 貼る絵は [`schorl_capture::FrameSource`] から、板専用の
//!   出力を名指しして取る。
//! - `code.idempotency.write` — 外へ出す事象はすべて [`schorl_core::id::IdGen`] が
//!   発行した鍵を持つ。
//! - `code.time.tz` — 事象の時刻は [`schorl_core::time::Clock`] の UTC。
//! - `house.resource_lifecycle.release_paths` — [`PanelDriver::run`] は成功でも
//!   失敗でも [`XrSession::end`] を通る。

use schorl_capture::FrameSource;
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::{IdGen, IdempotencyKey, TraceId};
use schorl_core::time::Clock;
use schorl_display::OutputId;
use schorl_input::{
    Delivery, KeyEvent, KeyboardSink, Keycode, PointerButton, PointerEvent, PointerEventKind,
    PointerSink,
};
use schorl_panel::cursor::{CursorResolution, PointerHold, resolve_cursor, to_pixels};
use schorl_panel::grab::{
    ControllerId, GrabState, begin_grab, panel_pose_while_held, release_grab,
};
use schorl_panel::math::Pose;
use schorl_panel::panel::Panel;

use schorl_xr::composition::CompositionPlan;
use schorl_xr::sleep::Sleeper;
use schorl_xr::{PressState, SessionConfig, SessionLoop, SessionState, XrEvent, XrSession};

/// 手ごとの最後に見た姿勢。
///
/// 掴みは掴んだ瞬間の姿勢を要るので、事象が別々に来ても組めるように覚えておく。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ControllerPoses {
    left: Option<Pose>,
    right: Option<Pose>,
}

impl ControllerPoses {
    /// 何も見ていない状態。
    pub const fn new() -> Self {
        Self {
            left: None,
            right: None,
        }
    }

    /// 覚える。
    pub const fn set(&mut self, controller: ControllerId, pose: Pose) {
        match controller {
            ControllerId::Left => self.left = Some(pose),
            ControllerId::Right => self.right = Some(pose),
        }
    }

    /// 覚えている姿勢。
    pub const fn get(&self, controller: ControllerId) -> Option<Pose> {
        match controller {
            ControllerId::Left => self.left,
            ControllerId::Right => self.right,
        }
    }
}

/// driver の構え。どれも `free` な軸の選択である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverConfig {
    /// カーソルを動かす手 (`free schorl.controller.binding_layout`)。
    pub pointer_controller: ControllerId,
    /// クリックとして Linux へ戻すボタン (`free schorl.controller.binding_layout`)。
    pub click_button: PointerButton,
    /// 一巡ごとに眠る長さ (`free schorl.panel.refresh_rate`)。
    pub step_interval_millis: u64,
}

impl DriverConfig {
    /// v1 の既定。どれも選択であって pin ではない。
    pub const DEFAULT: DriverConfig = DriverConfig {
        pointer_controller: ControllerId::Right,
        click_button: PointerButton::Left,
        step_interval_millis: 16,
    };
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// driver が要る capability だけを束ねた袋。
///
/// 一つの神 trait に押し込まず、最小の trait を並べて借りる
/// (`house.effect_boundary.no_god_capability`)。束ねているのは配線のためで、
/// 個々の trait は互いに独立している。
pub struct PanelWiring<'a> {
    /// 事象に押す UTC 時刻。
    pub clock: &'a dyn Clock,
    /// 冪等鍵の発行。
    pub ids: &'a dyn IdGen,
    /// 板に貼る絵の取り口。
    pub frames: &'a mut dyn FrameSource,
    /// ポインタを Linux へ戻す口。
    pub pointer: &'a mut dyn PointerSink,
    /// キーを Linux へ戻す口。
    pub keyboard: &'a mut dyn KeyboardSink,
    /// 一巡ごとの眠り。
    pub sleeper: &'a dyn Sleeper,
}

impl core::fmt::Debug for PanelWiring<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PanelWiring { .. }")
    }
}

/// 一巡で何が起きたか。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StepOutcome {
    /// 取り込んだ出来事の数。
    pub applied_events: usize,
    /// 決まった板上の位置。手の姿勢を一度も見ていなければ `None`。
    pub cursor: Option<CursorResolution>,
    /// Linux へ届いたポインタ事象の数 (重複で落ちた分は数えない)。
    pub pointer_deliveries: usize,
    /// Linux へ届いたキー事象の数。
    pub key_deliveries: usize,
    /// 組んだ合成計画。貼らなかった巡では `None`。
    pub plan: Option<CompositionPlan>,
    /// いまのセッションの生死。
    pub state: SessionState,
    /// 終わってよいか。
    pub stopping: bool,
}

/// 板一枚を回す driver。
pub struct PanelDriver<'a> {
    wiring: PanelWiring<'a>,
    config: DriverConfig,
    panel: Panel,
    output: OutputId,
    grab: GrabState,
    hold: PointerHold,
    poses: ControllerPoses,
    state: SessionState,
}

impl core::fmt::Debug for PanelDriver<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PanelDriver")
            .field("config", &self.config)
            .field("panel", &self.panel)
            .field("output", &self.output)
            .field("grab", &self.grab)
            .field("hold", &self.hold)
            .field("poses", &self.poses)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<'a> PanelDriver<'a> {
    /// 配線と板と出力を与えて組む。板は一枚しか渡せない (`v1.panel_count`)。
    pub const fn new(
        wiring: PanelWiring<'a>,
        config: DriverConfig,
        panel: Panel,
        output: OutputId,
    ) -> Self {
        Self {
            wiring,
            config,
            panel,
            output,
            grab: GrabState::Released,
            hold: PointerHold::Up,
            poses: ControllerPoses::new(),
            state: SessionState::Idle,
        }
    }

    /// いまの板。
    pub const fn panel(&self) -> &Panel {
        &self.panel
    }

    /// いまの掴み。
    pub const fn grab(&self) -> GrabState {
        self.grab
    }

    /// いまの押下。
    pub const fn hold(&self) -> PointerHold {
        self.hold
    }

    /// いまのセッションの生死。
    pub const fn state(&self) -> SessionState {
        self.state
    }

    /// セッションの構え。背景は黒、板は一枚。
    pub const fn session_config(&self) -> SessionConfig {
        SessionConfig::for_panel(self.panel)
    }

    /// 板の上の位置をいま決める。
    ///
    /// 板平面への射影だけで決まる (`ux.cursor_mapping`)。手の姿勢を一度も見て
    /// いなければ `None`。
    pub fn cursor(&self) -> Option<CursorResolution> {
        let pose = self.poses.get(self.config.pointer_controller)?;
        Some(resolve_cursor(&self.panel, pose.position, self.hold))
    }

    fn next_key(&self) -> Result<IdempotencyKey> {
        self.wiring.ids.next_id().map(IdempotencyKey::new)
    }

    /// 巡の上限を決めて回す。
    ///
    /// ランタイムが `Stopping` を上げないまま上限に達したら封筒で返す。止まらない
    /// セッションに縛られたくない呼び側のために在る。[`SessionLoop::run`] と同じく
    /// 成功でも失敗でも [`XrSession::end`] を通る。
    pub fn run_for_at_most(&mut self, session: &mut dyn XrSession, steps: usize) -> Result<usize> {
        let looped = (|| {
            for taken in 0..steps {
                let outcome = self.step(session)?;
                if outcome.stopping {
                    return Ok(taken + 1);
                }
                self.wiring
                    .sleeper
                    .sleep_millis(self.config.step_interval_millis);
            }
            Err(step_budget_exhausted(steps))
        })();
        let ended = session.end();
        match (looped, ended) {
            (Ok(taken), Ok(())) => Ok(taken),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    /// 一巡回す。
    ///
    /// 出来事を取り込み、板と押下を更新し、決まった位置とクリックとキーを外へ
    /// 渡し、貼る面があれば絵を取って貼る。
    pub fn step(&mut self, session: &mut dyn XrSession) -> Result<StepOutcome> {
        let mut outcome = StepOutcome {
            applied_events: 0,
            cursor: None,
            pointer_deliveries: 0,
            key_deliveries: 0,
            plan: None,
            state: self.state,
            stopping: false,
        };

        for event in session.poll_events()? {
            outcome.applied_events = outcome.applied_events.saturating_add(1);
            match event {
                XrEvent::StateChanged(state) => {
                    self.state = state;
                    if state == SessionState::Stopping {
                        outcome.stopping = true;
                    }
                }
                XrEvent::ControllerPose { controller, pose } => {
                    self.poses.set(controller, pose);
                    if self.grab.controller() == Some(controller)
                        && let Some(panel_pose) = panel_pose_while_held(self.grab, pose)
                    {
                        self.panel = self.panel.with_pose(panel_pose);
                    }
                }
                XrEvent::GrabButton { controller, state } => match state {
                    PressState::Pressed => {
                        if let Some(pose) = self.poses.get(controller) {
                            self.grab = begin_grab(controller, pose, self.panel.pose());
                        }
                    }
                    PressState::Released => {
                        // 掴んでいる手が離したときだけ離す。もう一方の手の離しで
                        // 板が落ちない。
                        if self.grab.controller() == Some(controller) {
                            self.grab = release_grab();
                        }
                    }
                },
                XrEvent::PointerButton { controller, state } => {
                    // クリックはカーソルを持つ手のものだけを Linux へ戻す。
                    // もう一方の手の押下でカーソル位置に click が落ちると、押した
                    // 手と当たる場所が食い違う。どちらの手がカーソルかは
                    // `free schorl.controller.binding_layout` の選択である。
                    if controller != self.config.pointer_controller {
                        continue;
                    }
                    self.hold = match state {
                        PressState::Pressed => PointerHold::Down,
                        PressState::Released => PointerHold::Up,
                    };
                    let click = PointerEvent {
                        idempotency_key: self.next_key()?,
                        at: self.wiring.clock.now_utc(),
                        kind: PointerEventKind::Button {
                            button: self.config.click_button,
                            state: state.to_input_state(),
                        },
                    };
                    if self.wiring.pointer.deliver(&click)? == Delivery::Delivered {
                        outcome.pointer_deliveries = outcome.pointer_deliveries.saturating_add(1);
                    }
                }
                XrEvent::Key { keycode, state } => {
                    let key = KeyEvent {
                        idempotency_key: self.next_key()?,
                        at: self.wiring.clock.now_utc(),
                        keycode,
                        state: state.to_input_state(),
                    };
                    if self.wiring.keyboard.deliver(&key)? == Delivery::Delivered {
                        outcome.key_deliveries = outcome.key_deliveries.saturating_add(1);
                    }
                }
            }
        }
        outcome.state = self.state;

        let cursor = self.cursor();
        outcome.cursor = cursor;
        if let Some(point) = cursor.and_then(CursorResolution::point) {
            let pixels = to_pixels(&self.panel, point);
            let motion = PointerEvent {
                idempotency_key: self.next_key()?,
                at: self.wiring.clock.now_utc(),
                // 0.2 以前はここで貼り先の出力 (`self.output`) を名指ししていた。
                // 原文3 でホストへ注入する経路が退役し、`PointerEventKind` から欄が
                // 消えたので、運ぶのは板上の画素位置だけ。`self.output` は絵を取る側
                // (`frames.capture`) でそのまま使われている。
                kind: PointerEventKind::MotionAbsolute {
                    x_px: pixels.x,
                    y_px: pixels.y,
                },
            };
            if self.wiring.pointer.deliver(&motion)? == Delivery::Delivered {
                outcome.pointer_deliveries = outcome.pointer_deliveries.saturating_add(1);
            }
        }

        if session.presents_frames() {
            let frame = self.wiring.frames.capture(&self.output)?;
            let plan = CompositionPlan::for_frame(&self.session_config(), &frame)?;
            session.submit_frame(&frame, &self.panel)?;
            outcome.plan = Some(plan);
        }

        Ok(outcome)
    }
}

/// `keycode` を綴るための補助。呼ぶ側が [`Keycode`] を組み立てやすくするだけ。
pub const fn keycode(raw: u32) -> Keycode {
    Keycode(raw)
}

impl SessionLoop for PanelDriver<'_> {
    /// 止まるまで回す。
    ///
    /// 一巡ごとに眠る。`XR_MND_headless` が busy-wait を避けるよう述べているため
    /// ([`crate::sleep`] の逐語)。成功でも失敗でも [`XrSession::end`] を通る
    /// (`house.resource_lifecycle.release_paths`)。
    fn run(&mut self, session: &mut dyn XrSession) -> Result<()> {
        let looped = loop {
            match self.step(session) {
                Ok(outcome) => {
                    if outcome.stopping {
                        break Ok(());
                    }
                    self.wiring
                        .sleeper
                        .sleep_millis(self.config.step_interval_millis);
                }
                Err(error) => break Err(error),
            }
        };
        let ended = session.end();
        match (looped, ended) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
        }
    }
}

/// セッションが止まらないまま巡が尽きたときの封筒。
///
/// 試験が無限に回らないようにするための上限を呼ぶ側が持てるようにする。
pub fn step_budget_exhausted(steps: usize) -> Error {
    Error::new(
        ErrorCode::Internal,
        "the session did not reach a stopping state within the step budget",
        TraceId::unattributed(),
    )
    .with_detail("steps", steps as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_capture::testing::SolidColourFrameSource;
    use schorl_core::id::IdScheme;
    use schorl_core::testing::{FixedClock, SequentialIdGen};
    use schorl_core::time::UtcTimestamp;
    use schorl_input::ButtonState;
    use schorl_input::testing::RecordingSink;
    use schorl_panel::math::{Quat, Vec3};
    use schorl_panel::panel::{PanelPose, PanelResolution, PanelSize};
    use schorl_xr::testing::ScriptedSession;

    use schorl_xr::sleep::testing::CountingSleeper;

    fn test_panel() -> Panel {
        Panel::single(
            PanelSize::new(1.2, 0.675).expect("valid size"),
            PanelResolution::new(8, 4).expect("valid resolution"),
            PanelPose::world(Pose::new(Vec3::new(0.0, 0.0, -1.0), Quat::IDENTITY)),
        )
    }

    struct Harness {
        clock: FixedClock,
        ids: SequentialIdGen,
        frames: SolidColourFrameSource,
        pointer: RecordingSink,
        keyboard: RecordingSink,
        sleeper: CountingSleeper,
        output: OutputId,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                clock: FixedClock::at_millis(1_700_000_000_000),
                ids: SequentialIdGen::new(IdScheme::Ulid, "drv"),
                frames: SolidColourFrameSource {
                    width_px: 8,
                    height_px: 4,
                    fill: 0x11,
                    captured_at: UtcTimestamp::from_millis_since_epoch(1),
                },
                pointer: RecordingSink::new(),
                keyboard: RecordingSink::new(),
                sleeper: CountingSleeper::new(),
                output: OutputId::new("SCHORL-PANEL").expect("valid name"),
            }
        }

        fn driver(&mut self) -> PanelDriver<'_> {
            PanelDriver::new(
                PanelWiring {
                    clock: &self.clock,
                    ids: &self.ids,
                    frames: &mut self.frames,
                    pointer: &mut self.pointer,
                    keyboard: &mut self.keyboard,
                    sleeper: &self.sleeper,
                },
                DriverConfig::DEFAULT,
                test_panel(),
                self.output.clone(),
            )
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn a_grabbed_panel_follows_the_hand_and_stays_where_it_is_let_go() {
        let mut harness = Harness::new();
        let mut driver = harness.driver();
        let start = Pose::new(Vec3::new(0.0, 0.0, -0.4), Quat::IDENTITY);

        let mut session = ScriptedSession {
            scripted_events: vec![
                XrEvent::ControllerPose {
                    controller: ControllerId::Right,
                    pose: start,
                },
                XrEvent::GrabButton {
                    controller: ControllerId::Right,
                    state: PressState::Pressed,
                },
                XrEvent::ControllerPose {
                    controller: ControllerId::Right,
                    pose: Pose::new(Vec3::new(0.5, 0.2, -0.4), Quat::IDENTITY),
                },
            ],
            ..Default::default()
        };

        driver.step(&mut session).expect("stepped");
        assert!(driver.grab().is_held());
        let moved = driver.panel().pose().pose.position;
        assert!(close(moved.x, 0.5) && close(moved.y, 0.2), "got {moved:?}");
        assert!(close(moved.z, -1.0), "depth follows the hand: {moved:?}");

        session.scripted_events = vec![
            XrEvent::GrabButton {
                controller: ControllerId::Right,
                state: PressState::Released,
            },
            XrEvent::ControllerPose {
                controller: ControllerId::Right,
                pose: Pose::new(Vec3::new(9.0, 9.0, 9.0), Quat::IDENTITY),
            },
        ];
        driver.step(&mut session).expect("stepped");
        assert!(!driver.grab().is_held());
        assert_eq!(
            driver.panel().pose().pose.position,
            moved,
            "the panel is left in the world, not carried by the hand"
        );
    }

    #[test]
    fn the_other_hand_releasing_does_not_drop_the_panel() {
        let mut harness = Harness::new();
        let mut driver = harness.driver();
        let mut session = ScriptedSession {
            scripted_events: vec![
                XrEvent::ControllerPose {
                    controller: ControllerId::Left,
                    pose: Pose::new(Vec3::new(-0.2, 0.0, -0.4), Quat::IDENTITY),
                },
                XrEvent::GrabButton {
                    controller: ControllerId::Left,
                    state: PressState::Pressed,
                },
                XrEvent::GrabButton {
                    controller: ControllerId::Right,
                    state: PressState::Released,
                },
            ],
            ..Default::default()
        };
        driver.step(&mut session).expect("stepped");
        assert_eq!(driver.grab().controller(), Some(ControllerId::Left));
    }

    #[test]
    fn the_cursor_is_the_projection_of_the_hand_onto_the_panel_plane() {
        let mut harness = Harness::new();
        let mut driver = harness.driver();
        // 板は z = -1 にあり、手は板から 0.6m 手前で右に 0.3m ずれている。
        let mut session = ScriptedSession {
            scripted_events: vec![XrEvent::ControllerPose {
                controller: ControllerId::Right,
                pose: Pose::new(Vec3::new(0.3, 0.1, -0.4), Quat::IDENTITY),
            }],
            ..Default::default()
        };
        let outcome = driver.step(&mut session).expect("stepped");
        let point = outcome
            .cursor
            .expect("a hand was seen")
            .point()
            .expect("on the panel");
        assert!(close(point.u_m, 0.3) && close(point.v_m, 0.1), "{point:?}");

        // 板までの距離を変えても板上の位置は動かない。射影だから。
        session.scripted_events = vec![XrEvent::ControllerPose {
            controller: ControllerId::Right,
            pose: Pose::new(Vec3::new(0.3, 0.1, -0.9), Quat::IDENTITY),
        }];
        let outcome = driver.step(&mut session).expect("stepped");
        let same = outcome
            .cursor
            .expect("a hand was seen")
            .point()
            .expect("on the panel");
        assert_eq!(point, same);
    }

    #[test]
    fn the_cursor_keeps_following_past_the_edge_only_while_held() {
        let mut harness = Harness::new();
        let mut driver = harness.driver();
        let far = Pose::new(Vec3::new(4.0, 0.0, -0.4), Quat::IDENTITY);

        let mut session = ScriptedSession {
            scripted_events: vec![
                XrEvent::ControllerPose {
                    controller: ControllerId::Right,
                    pose: far,
                },
                XrEvent::PointerButton {
                    controller: ControllerId::Right,
                    state: PressState::Pressed,
                },
            ],
            ..Default::default()
        };
        let outcome = driver.step(&mut session).expect("stepped");
        assert_eq!(driver.hold(), PointerHold::Down);
        assert!(matches!(
            outcome.cursor,
            Some(CursorResolution::BeyondEdgeWhileHeld(_))
        ));
        // 押している間は縁の外でも動きが Linux へ出る (クリック + 動きで 2)。
        assert_eq!(outcome.pointer_deliveries, 2);

        session.scripted_events = vec![XrEvent::PointerButton {
            controller: ControllerId::Right,
            state: PressState::Released,
        }];
        let outcome = driver.step(&mut session).expect("stepped");
        assert_eq!(driver.hold(), PointerHold::Up);
        assert_eq!(outcome.cursor, Some(CursorResolution::OffPanel));
        // 離したので動きは出ない。出たのは離したクリックだけ。
        assert_eq!(outcome.pointer_deliveries, 1);
    }

    #[test]
    fn a_click_and_a_key_reach_the_linux_side() {
        let mut harness = Harness::new();
        {
            let mut driver = harness.driver();
            let mut session = ScriptedSession {
                scripted_events: vec![
                    XrEvent::PointerButton {
                        controller: ControllerId::Right,
                        state: PressState::Pressed,
                    },
                    XrEvent::Key {
                        keycode: keycode(30),
                        state: PressState::Pressed,
                    },
                    XrEvent::Key {
                        keycode: keycode(30),
                        state: PressState::Released,
                    },
                ],
                ..Default::default()
            };
            let outcome = driver.step(&mut session).expect("stepped");
            assert_eq!(outcome.pointer_deliveries, 1, "no hand seen, so no motion");
            assert_eq!(outcome.key_deliveries, 2);
        }

        assert_eq!(harness.pointer.pointer_events().len(), 1);
        match &harness.pointer.pointer_events()[0].kind {
            PointerEventKind::Button { button, state } => {
                assert_eq!(*button, PointerButton::Left);
                assert_eq!(*state, ButtonState::Pressed);
            }
            PointerEventKind::MotionAbsolute { .. } => unreachable!("delivered as a button"),
        }
        let keys = harness.keyboard.key_events();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].keycode, Keycode(30));
        assert_eq!(keys[0].state, ButtonState::Pressed);
        assert_eq!(keys[1].state, ButtonState::Released);
        assert_eq!(
            keys[0].at,
            UtcTimestamp::from_millis_since_epoch(1_700_000_000_000),
            "the clock is the injected UTC one"
        );
        assert_ne!(
            keys[0].idempotency_key, keys[1].idempotency_key,
            "every outward write carries its own key"
        );
    }

    #[test]
    fn the_motion_delivered_carries_the_panel_pixels() {
        let mut harness = Harness::new();
        {
            let mut driver = harness.driver();
            let mut session = ScriptedSession {
                scripted_events: vec![XrEvent::ControllerPose {
                    controller: ControllerId::Right,
                    pose: Pose::new(Vec3::new(0.0, 0.0, -0.4), Quat::IDENTITY),
                }],
                ..Default::default()
            };
            driver.step(&mut session).expect("stepped");
        }
        let events = harness.pointer.pointer_events();
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            PointerEventKind::MotionAbsolute { x_px, y_px } => {
                // 8x4 の板の中心。
                assert_eq!((*x_px, *y_px), (4, 2));
            }
            PointerEventKind::Button { .. } => unreachable!("delivered as motion"),
        }
    }

    #[test]
    fn a_presenting_session_gets_a_black_backed_plan_and_the_captured_frame() {
        let mut harness = Harness::new();
        let mut session = ScriptedSession::default();
        {
            let mut driver = harness.driver();
            let outcome = driver.step(&mut session).expect("stepped");
            let plan = outcome.plan.expect("a presenting session gets a plan");
            assert!(plan.background_is_opaque_black());
            assert_eq!(plan.panel_layer_count(), 1);
        }
        assert_eq!(session.submitted_frames, 1);
    }

    #[test]
    fn a_headless_session_is_driven_without_pasting_anything() {
        let mut harness = Harness::new();
        let mut session = ScriptedSession {
            presents: false,
            ..Default::default()
        };
        let mut driver = harness.driver();
        let outcome = driver.step(&mut session).expect("stepped");
        assert!(outcome.plan.is_none());
        assert_eq!(session.submitted_frames, 0);
    }

    #[test]
    fn running_stops_on_the_stopping_state_and_always_ends_the_session() {
        let mut harness = Harness::new();
        let mut session = ScriptedSession {
            presents: false,
            scripted_events: vec![
                XrEvent::StateChanged(SessionState::Running),
                XrEvent::StateChanged(SessionState::Stopping),
            ],
            ..Default::default()
        };
        {
            let mut driver = harness.driver();
            driver.run(&mut session).expect("ran to a stop");
            assert_eq!(driver.state(), SessionState::Stopping);
        }
        assert_eq!(session.ended, 1);
        assert!(
            harness.sleeper.requested().is_empty(),
            "it stopped on the first step, so it never slept"
        );
    }

    #[test]
    fn running_sleeps_between_steps_instead_of_spinning() {
        let mut harness = Harness::new();
        let mut session = ScriptedSession {
            presents: false,
            scripted_events: Vec::new(),
            stop_after_polls: Some(3),
            ..Default::default()
        };
        {
            let mut driver = harness.driver();
            driver.run(&mut session).expect("ran to a stop");
        }
        assert_eq!(
            harness.sleeper.requested(),
            vec![16, 16],
            "it slept between the steps that did not stop"
        );
        assert_eq!(session.ended, 1);
    }

    #[test]
    fn a_failing_step_still_ends_the_session() {
        let mut harness = Harness::new();
        let mut session = ScriptedSession {
            presents: false,
            poll_error: true,
            ..Default::default()
        };
        {
            let mut driver = harness.driver();
            let err = driver.run(&mut session).expect_err("polling fails");
            assert_eq!(err.code(), ErrorCode::Internal);
        }
        assert_eq!(session.ended, 1, "the release path runs on failure too");
    }

    #[test]
    fn a_click_from_the_other_hand_is_not_delivered() {
        let mut harness = Harness::new();
        {
            let mut driver = harness.driver();
            let mut session = ScriptedSession {
                scripted_events: vec![XrEvent::PointerButton {
                    controller: ControllerId::Left,
                    state: PressState::Pressed,
                }],
                ..Default::default()
            };
            let outcome = driver.step(&mut session).expect("stepped");
            assert_eq!(outcome.pointer_deliveries, 0);
            assert_eq!(
                driver.hold(),
                PointerHold::Up,
                "the other hand does not start a drag"
            );
        }
        assert!(harness.pointer.pointer_events().is_empty());
    }

    #[test]
    fn a_bounded_run_stops_on_the_stopping_state() {
        let mut harness = Harness::new();
        let mut session = ScriptedSession {
            presents: false,
            stop_after_polls: Some(2),
            ..Default::default()
        };
        {
            let mut driver = harness.driver();
            assert_eq!(
                driver
                    .run_for_at_most(&mut session, 10)
                    .expect("stops within the budget"),
                2
            );
        }
        assert_eq!(session.ended, 1);
    }

    #[test]
    fn a_bounded_run_refuses_a_session_that_never_stops() {
        let mut harness = Harness::new();
        let mut session = ScriptedSession {
            presents: false,
            ..Default::default()
        };
        {
            let mut driver = harness.driver();
            let err = driver
                .run_for_at_most(&mut session, 3)
                .expect_err("it never stops");
            assert_eq!(err.code(), ErrorCode::Internal);
        }
        assert_eq!(session.ended, 1, "the release path runs on the budget path");
        assert_eq!(harness.sleeper.requested(), vec![16, 16, 16]);
    }

    #[test]
    fn the_step_budget_envelope_reports_the_count() {
        let err = step_budget_exhausted(42);
        assert_eq!(err.code(), ErrorCode::Internal);
    }
}
