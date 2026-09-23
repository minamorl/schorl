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
//! **v1 の組み立ては [`SchorlSession`] である。** compositor 面と描画面を一本に
//! 繋ぎ、繋いできた toplevel を 360 度の空間へ置く
//! (`space.content_unit` / `v1.window_count` / `v1.window_displayed`)。
//!
//! **[`Schorl`] と [`Capabilities`] は板一枚の頃の組み立てであり、v1 の経路には
//! 無い。** spec 0.2 の原文3 で、空間に置かれる単位はディスプレイからウィンドウへ
//! 移り (`space.content_unit = window`)、上限一枚を凍らせていた `v1.panel_count` は
//! `v1.window_count = one_or_more` へ差し替わった。上限一枚は御主人様の原文では
//! なく結衣が置いた scope cap だった、というのが退役の理由である
//! (spec の `@meta withdrawn_v0_2` (3))。
//!
//! 消していないのは `schorl-capture` / `schorl-display` / `schorl-panel-driver` を
//! 消していないのと同じ理由で、下に在る試験が見ている不変条件は
//! `v1.panel_count` を除いて今も生きているため。掴み・置き直し・カーソルの三本は
//! 板語彙からウィンドウ語彙へ鍵を移しただけで意味が変わっていない
//! (spec の `@meta withdrawn_v0_2` (5) と (8)、逐語「三本の意味は不変」)。
//! **ここから v1 のバイナリへは一本も辺が出ていない。** 借りている計算の正本は
//! `schorl-panel` に在り、そちらは v1 の経路も通る。
//!
//! **spec 0.2 の原文3 で、この境界から捕捉 (`schorl-capture`) と
//! ホスト出力の貸し借り (`schorl-display`) が外れた。** schorl 自身が compositor に
//! なってクライアントを直接持つなら、他 compositor の画面を取る口も、ホストに
//! 出力を作らせる口も要らない。両 crate は実測資産として workspace に残っており、
//! 退役した loop は `schorl-panel-driver` に在る。v1 のバイナリはどちらも通らない。

use schorl_core::error::Result;
use schorl_core::id::{IdGen, IdempotencyKey, TraceId};
use schorl_core::log::{Level, LogRecord, LogSink};
use schorl_core::time::{Clock, UtcTimestamp};
use schorl_input::{KeyboardSink, PointerEvent, PointerEventKind, PointerSink};
use schorl_panel::cursor::{CursorResolution, PointerHold, resolve_cursor, to_pixels};
use schorl_panel::grab::{
    ControllerId, GrabState, begin_grab, panel_pose_while_held, release_grab,
};
use schorl_panel::math::{Pose, Vec3};
use schorl_panel::panel::Panel;
use schorl_xr::{SessionConfig, XrRuntime};

pub mod stdout_log;

// --- v1 の配線 ---------------------------------------------------------------
// ここから下は「compositor 面と描画面を一本に繋ぐ」ための面である。
// どちらの crate も相手を知らないので、両方を知るのはこの境界だけになる
// (`house.effect_boundary.location = injected_at_boundary`)。
pub mod grab;
pub mod hands;
pub mod pixels;
pub mod probe;
pub mod session;
pub mod stage;

pub use grab::{GrabEffect, WindowGrabs, WindowPlacement};
pub use hands::{BoundSources, HandInput};
pub use pixels::{ClientBuffer, ClientPixelRelay, PixelInbox};
pub use session::{SchorlSession, SessionOptions, StepOutcome};
pub use stage::{Stage, StageReport};
pub use stdout_log::StdoutJsonLogSink;

/// 注入する capability 一式。**板一枚の頃の面で、v1 の経路には無い。**
///
/// 一つの trait に押し込まず、最小の trait を並べて持つ。
/// v1 の組み立てが何を借りるかは [`SessionOptions`] が持つ。
pub struct Capabilities {
    /// 時刻。
    pub clock: Box<dyn Clock>,
    /// 識別子の発行。
    pub ids: Box<dyn IdGen>,
    /// ログの行き先。
    pub log: Box<dyn LogSink>,
    /// ポインタをクライアントへ配る口。
    pub pointer: Box<dyn PointerSink>,
    /// キー入力をクライアントへ配る口。
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
///
/// **退役した面である。** 原文3 でこの構えは v1 から外れた (crate の冒頭を見よ)。
/// v1 の組み立ては [`SchorlSession`]。
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

    /// 可変で借りる。入力を配る口は `&mut self` を要る。
    pub const fn capabilities_mut(&mut self) -> &mut Capabilities {
        &mut self.caps
    }

    /// セッションの構え。背景は黒 (`space.background`)、板は一枚。
    ///
    /// 「一枚」を縛っていた `v1.panel_count` は原文3 で退役し、いま効いているのは
    /// `v1.window_count = one_or_more` である。**この面が一枚しか持てないことは、
    /// v1 の上限が一枚だという意味ではない。** 上限を持たないのは
    /// [`SchorlSession`] の側である。
    pub const fn session_config(&self) -> SessionConfig {
        SessionConfig::for_panel(self.panel)
    }

    /// コントローラの位置を板平面へ落としてカーソルを決める。
    ///
    /// `ux.cursor_mapping` / `ux.cursor_beyond_edge` は生きている pin だが、鍵は
    /// 板からウィンドウへ移っている。ここが落とす先は板平面であって
    /// ウィンドウ平面ではない。**満たしているのは同じ不変条件の板側の写しである。**
    pub fn cursor(&self, controller_point: Vec3, hold: PointerHold) -> CursorResolution {
        resolve_cursor(&self.panel, controller_point, hold)
    }

    /// カーソルの解決結果を、クライアントへ配るポインタ事象へ直す。
    ///
    /// 板から外れて押してもいないときは何も作らない。作らないことが
    /// 「板の上でだけ動く」という形になる。
    pub fn pointer_motion(
        &self,
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
                x_px: pixels.x,
                y_px: pixels.y,
            },
        })
    }

    /// 板を掴む。`v1.panel_grab` は原文3 で `v1.window_grab` へ改鍵された
    /// (鍵だけで、掴む主体がコントローラであることは変わっていない)。
    pub fn begin_panel_grab(&mut self, controller: ControllerId, controller_pose: Pose) {
        self.grab = begin_grab(controller, controller_pose, self.panel.pose());
    }

    /// 掴んでいる間、板を手に追従させる。離していれば何もしない。
    pub fn follow_grab(&mut self, controller_pose: Pose) {
        if let Some(pose) = panel_pose_while_held(self.grab, controller_pose) {
            self.panel = self.panel.with_pose(pose);
        }
    }

    /// 離す。板はその場に残る。`ux.panel_not_head_locked` は原文3 で
    /// `ux.window_not_head_locked` へ改鍵された (鍵だけで、意味は不変)。
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

/// 板一枚の頃の組み立てを見る試験。
///
/// **ここが測っているのは [`Schorl`] であって v1 の経路ではない。** 六本のうち
/// 五本が見ている不変条件 (黒い背景・板平面への射影・縁の外の追従・掴んで離した
/// あとに残ること・json 一行のログ) は、鍵がウィンドウ語彙へ移っただけで spec 0.4
/// でも生きている。残る一本だけが退役した `v1.panel_count` を見ており、名前で
/// そう断ってある。
#[cfg(test)]
mod tests {
    use super::*;
    use schorl_core::id::IdScheme;
    use schorl_core::testing::{CapturingLogSink, FixedClock, SequentialIdGen};
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
            pointer: Box::new(schorl_input::testing::RecordingSink::new()),
            keyboard: Box::new(schorl_input::testing::RecordingSink::new()),
            xr: Box::new(ScriptedRuntime::default()),
        };
        Schorl::new(caps, Panel::default_single())
    }

    /// 黒い背景は生きている pin (`space.background`)。板が一枚なのは退役した
    /// `v1.panel_count` であって、v1 の上限ではない。
    #[test]
    fn the_retired_panel_wiring_is_black_with_one_panel() {
        let schorl = wired();
        let config = schorl.session_config();
        assert_eq!(config.background, Background::Black);
        assert_eq!(config.panel.count(), 1);
    }

    #[test]
    fn a_cursor_over_the_panel_becomes_a_motion_event() {
        let schorl = wired();
        let centre = schorl.panel().pose().pose.position;
        let cursor = schorl.cursor(centre, PointerHold::Up);
        let key = IdempotencyKey::new(
            schorl_core::id::Id::new(IdScheme::Ulid, "move-1").expect("valid text"),
        );
        let event = schorl
            .pointer_motion(cursor, key, UtcTimestamp::from_millis_since_epoch(1))
            .expect("the cursor is on the panel");
        match event.kind {
            PointerEventKind::MotionAbsolute { x_px, y_px } => {
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
        let key = IdempotencyKey::new(
            schorl_core::id::Id::new(IdScheme::Ulid, "move-2").expect("valid text"),
        );
        assert!(
            schorl
                .pointer_motion(cursor, key, UtcTimestamp::from_millis_since_epoch(1))
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
