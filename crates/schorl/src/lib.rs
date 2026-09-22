//! `schorl` — 組み立ての境界。
//!
//! 周囲効果を持つ実装は、すべてここで一度だけ注入される
//! (`house.effect_boundary.location: require ... = injected_at_boundary`)。
//! 下の crate は trait しか知らないので、贋物と本物を入れ替えても形が変わらない。
//!
//! [`Capabilities`] は capability を束ねる袋であって、神 capability ではない。
//! 束ねているのは配線のためで、個々の trait は互いに独立している
//! (`house.effect_boundary.no_god_capability`)。
//!
//! この phase で在るのは配線と純粋な変換だけ。捕捉・注入・OpenXR の実装は
//! 後続 phase が [`schorl_capture::FrameSource`] / [`schorl_input::PointerSink`] /
//! [`schorl_xr::XrRuntime`] を実装して差し込む。

use schorl_capture::FrameSource;
use schorl_core::error::Result;
use schorl_core::id::{IdGen, IdempotencyKey, TraceId};
use schorl_core::log::{Level, LogRecord, LogSink};
use schorl_core::time::{Clock, UtcTimestamp};
use schorl_display::{OutputId, OutputRequest, VirtualOutputProvider};
use schorl_input::{KeyboardSink, PointerEvent, PointerEventKind, PointerSink};
use schorl_panel::cursor::{CursorResolution, PointerHold, resolve_cursor, to_pixels};
use schorl_panel::grab::{ControllerId, GrabState, begin_grab, panel_pose_while_held, release_grab};
use schorl_panel::math::{Pose, Vec3};
use schorl_panel::panel::Panel;
use schorl_xr::{SessionConfig, XrRuntime};

pub mod stdout_log;

pub use stdout_log::StdoutJsonLogSink;

/// 注入する capability 一式。
///
/// 一つの trait に押し込まず、最小の trait を並べて持つ。
pub struct Capabilities {
    /// 時刻。
    pub clock: Box<dyn Clock>,
    /// 識別子の発行。
    pub ids: Box<dyn IdGen>,
    /// ログの行き先。
    pub log: Box<dyn LogSink>,
    /// 仮想出力の貸し借り。
    pub outputs: Box<dyn VirtualOutputProvider>,
    /// 画面の捕捉。
    pub frames: Box<dyn FrameSource>,
    /// ポインタの注入。
    pub pointer: Box<dyn PointerSink>,
    /// キー入力の注入。
    pub keyboard: Box<dyn KeyboardSink>,
    /// XR ランタイム。
    pub xr: Box<dyn XrRuntime>,
}

impl core::fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Capabilities { .. }")
    }
}

/// 板一枚ぶんの状態と、注入された capability。
pub struct Schorl {
    caps: Capabilities,
    panel: Panel,
    grab: GrabState,
}

impl core::fmt::Debug for Schorl {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Schorl")
            .field("panel", &self.panel)
            .field("grab", &self.grab)
            .finish_non_exhaustive()
    }
}

impl Schorl {
    /// capability を注入して組み立てる。板は一枚だけ。
    pub const fn new(caps: Capabilities, panel: Panel) -> Self {
        Self {
            caps,
            panel,
            grab: GrabState::Released,
        }
    }

    /// いまの板。
    pub const fn panel(&self) -> &Panel {
        &self.panel
    }

    /// いまの掴みの状態。
    pub const fn grab(&self) -> GrabState {
        self.grab
    }

    /// 注入された capability。後続 phase のループが使う。
    pub const fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// 可変で借りる。捕捉と注入は `&mut self` を要る。
    pub const fn capabilities_mut(&mut self) -> &mut Capabilities {
        &mut self.caps
    }

    /// セッションの構え。背景は黒、板は一枚 (`space.background` / `v1.panel_count`)。
    pub const fn session_config(&self) -> SessionConfig {
        SessionConfig::for_panel(self.panel)
    }

    /// 板に合わせた仮想出力の注文 (`v1.panel_source`)。
    pub fn output_request(&self, idempotency_key: IdempotencyKey) -> OutputRequest {
        OutputRequest {
            preferred_name: "SCHORL-PANEL".to_owned(),
            width_px: self.panel.resolution().width_px,
            height_px: self.panel.resolution().height_px,
            refresh_millihz: 60_000,
            idempotency_key,
        }
    }

    /// コントローラの位置を板平面へ落としてカーソルを決める
    /// (`ux.cursor_mapping` / `ux.cursor_beyond_edge`)。
    pub fn cursor(&self, controller_point: Vec3, hold: PointerHold) -> CursorResolution {
        resolve_cursor(&self.panel, controller_point, hold)
    }

    /// カーソルの解決結果を、Linux へ戻すポインタ事象へ直す。
    ///
    /// 板から外れて押してもいないときは何も作らない。作らないことが
    /// 「板の上でだけ動く」という形になる。
    pub fn pointer_motion(
        &self,
        output: &OutputId,
        cursor: CursorResolution,
        idempotency_key: IdempotencyKey,
        at: UtcTimestamp,
    ) -> Option<PointerEvent> {
        let point = cursor.point()?;
        let pixels = to_pixels(&self.panel, point);
        Some(PointerEvent {
            idempotency_key,
            at,
            kind: PointerEventKind::MotionAbsolute {
                output: output.clone(),
                x_px: pixels.x,
                y_px: pixels.y,
            },
        })
    }

    /// 板を掴む (`v1.panel_grab`)。
    pub fn begin_panel_grab(&mut self, controller: ControllerId, controller_pose: Pose) {
        self.grab = begin_grab(controller, controller_pose, self.panel.pose());
    }

    /// 掴んでいる間、板を手に追従させる。離していれば何もしない。
    pub fn follow_grab(&mut self, controller_pose: Pose) {
        if let Some(pose) = panel_pose_while_held(self.grab, controller_pose) {
            self.panel = self.panel.with_pose(pose);
        }
    }

    /// 離す。板はその場に残る (`ux.panel_not_head_locked`)。
    pub fn end_panel_grab(&mut self) {
        self.grab = release_grab();
    }

    /// 追跡子つきで一行ログを出す。
    pub fn log(&self, level: Level, msg: impl Into<String>) -> Result<()> {
        let trace_id = match self.caps.ids.next_id() {
            Ok(id) => TraceId::new(id),
            Err(_) => TraceId::unattributed(),
        };
        let record = LogRecord::new(self.caps.clock.now_utc(), level, trace_id, msg);
        self.caps.log.emit(&record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_capture::testing::SolidColourFrameSource;
    use schorl_core::id::IdScheme;
    use schorl_core::testing::{CapturingLogSink, FixedClock, SequentialIdGen};
    use schorl_display::testing::{FakeOutputProvider, RecordingReleaser};
    use schorl_panel::math::Quat;
    use schorl_panel::panel::PanelPose;
    use schorl_scope::Background;
    use schorl_xr::testing::ScriptedRuntime;
    use std::sync::Arc;

    fn wired() -> Schorl {
        let caps = Capabilities {
            clock: Box::new(FixedClock::at_millis(1_700_000_000_000)),
            ids: Box::new(SequentialIdGen::new(IdScheme::Ulid, "test")),
            log: Box::new(CapturingLogSink::new()),
            outputs: Box::new(FakeOutputProvider::new(Arc::new(RecordingReleaser::new()))),
            frames: Box::new(SolidColourFrameSource {
                width_px: 4,
                height_px: 2,
                fill: 0,
                captured_at: UtcTimestamp::from_millis_since_epoch(0),
            }),
            pointer: Box::new(schorl_input::testing::RecordingSink::new()),
            keyboard: Box::new(schorl_input::testing::RecordingSink::new()),
            xr: Box::new(ScriptedRuntime::default()),
        };
        Schorl::new(caps, Panel::default_single())
    }

    #[test]
    fn the_wired_session_is_black_with_one_panel() {
        let schorl = wired();
        let config = schorl.session_config();
        assert_eq!(config.background, Background::Black);
        assert_eq!(config.panel.count(), 1);
    }

    #[test]
    fn the_output_request_matches_the_panel_resolution() {
        let schorl = wired();
        let key = IdempotencyKey::new(
            schorl_core::id::Id::new(IdScheme::Ulid, "req-1").expect("valid text"),
        );
        let request = schorl.output_request(key);
        assert_eq!(request.width_px, schorl.panel().resolution().width_px);
        assert_eq!(request.height_px, schorl.panel().resolution().height_px);
    }

    #[test]
    fn a_cursor_over_the_panel_becomes_a_motion_event() {
        let schorl = wired();
        let centre = schorl.panel().pose().pose.position;
        let cursor = schorl.cursor(centre, PointerHold::Up);
        let output = OutputId::new("SCHORL-PANEL").expect("valid name");
        let key = IdempotencyKey::new(
            schorl_core::id::Id::new(IdScheme::Ulid, "move-1").expect("valid text"),
        );
        let event = schorl
            .pointer_motion(
                &output,
                cursor,
                key,
                UtcTimestamp::from_millis_since_epoch(1),
            )
            .expect("the cursor is on the panel");
        match event.kind {
            PointerEventKind::MotionAbsolute { x_px, y_px, .. } => {
                assert_eq!((x_px, y_px), (960, 540));
            }
            PointerEventKind::Button { .. } => unreachable!("built as motion"),
        }
    }

    #[test]
    fn a_cursor_off_the_panel_without_a_hold_moves_nothing() {
        let schorl = wired();
        let far = Vec3::new(9.0, 1.3, -1.5);
        let cursor = schorl.cursor(far, PointerHold::Up);
        assert_eq!(cursor, CursorResolution::OffPanel);
        let output = OutputId::new("SCHORL-PANEL").expect("valid name");
        let key = IdempotencyKey::new(
            schorl_core::id::Id::new(IdScheme::Ulid, "move-2").expect("valid text"),
        );
        assert!(
            schorl
                .pointer_motion(
                    &output,
                    cursor,
                    key,
                    UtcTimestamp::from_millis_since_epoch(1)
                )
                .is_none()
        );
    }

    #[test]
    fn grabbing_moves_the_panel_and_releasing_leaves_it_there() {
        let mut schorl = wired();
        let start = Pose::new(Vec3::new(0.0, 1.0, -0.5), Quat::IDENTITY);
        schorl.begin_panel_grab(ControllerId::Right, start);
        assert!(schorl.grab().is_held());

        schorl.follow_grab(Pose::new(Vec3::new(0.4, 1.0, -0.5), Quat::IDENTITY));
        let moved = schorl.panel().pose().pose.position;
        assert!((moved.x - 0.4).abs() < 1e-4, "got {moved:?}");

        schorl.end_panel_grab();
        assert!(!schorl.grab().is_held());

        // 離したあとに手を動かしても板は残る。
        schorl.follow_grab(Pose::new(Vec3::new(5.0, 5.0, 5.0), Quat::IDENTITY));
        assert_eq!(schorl.panel().pose().pose.position, moved);
    }

    #[test]
    fn a_panel_moved_by_hand_stays_in_the_world_frame() {
        let mut schorl = wired();
        schorl.begin_panel_grab(ControllerId::Left, Pose::IDENTITY);
        schorl.follow_grab(Pose::new(Vec3::new(0.0, 0.2, 0.0), Quat::IDENTITY));
        assert_eq!(
            schorl.panel().pose().frame,
            PanelPose::world(Pose::IDENTITY).frame
        );
    }

    #[test]
    fn logging_emits_one_json_line_with_a_trace_id() {
        let sink = Arc::new(CapturingLogSink::new());
        let caps = Capabilities {
            clock: Box::new(FixedClock::at_millis(7)),
            ids: Box::new(SequentialIdGen::new(IdScheme::Ulid, "trace")),
            log: Box::new(sink.clone()),
            outputs: Box::new(FakeOutputProvider::new(Arc::new(RecordingReleaser::new()))),
            frames: Box::new(SolidColourFrameSource {
                width_px: 1,
                height_px: 1,
                fill: 0,
                captured_at: UtcTimestamp::from_millis_since_epoch(0),
            }),
            pointer: Box::new(schorl_input::testing::RecordingSink::new()),
            keyboard: Box::new(schorl_input::testing::RecordingSink::new()),
            xr: Box::new(ScriptedRuntime::default()),
        };
        let schorl = Schorl::new(caps, Panel::default_single());
        schorl.log(Level::Info, "wired").expect("emitted");

        let lines = sink.lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "{\"ts\":7,\"level\":\"info\",\"trace_id\":\"trace-00000000\",\"msg\":\"wired\"}"
        );
    }
}
