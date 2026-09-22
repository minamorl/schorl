//! `schorl-xr` — 被ったときに見える側。
//!
//! 満たす pin:
//! - `space.background: require schorl.virtual_space.background = black` —
//!   [`SessionConfig`] の背景は [`Background`] なので黒以外を組めない。
//! - `display.presence` / `display.purpose` — セッションは常に板一枚を伴う。
//! - `v1.panel_grab` — 掴みの入口は [`XrEvent::GrabButton`] と
//!   [`XrEvent::ControllerPose`] だけ。頭の姿勢から板を動かす口は無い。
//! - `verify.machine_scope` の `openxr_session_opens` — セッションを開く操作を
//!   [`XrRuntime::open_session`] という一本の署名にしてあるので、機械検査が
//!   そこだけを突ける。
//! - `house.resource_lifecycle.*` — [`XrSession`] は明示終了と `Drop` の両方で閉じる。
//!
//! どのランタイムを借りるか (`free schorl.runtime.*` / `free schorl.session.kind`)
//! はここでは決めない。**OpenXR の呼び出しはこの phase では書かない。**
//! 穴は trait の署名として残す。束縛の割り当ては `free schorl.controller.binding_layout`。

use schorl_capture::Frame;
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_panel::grab::ControllerId;
use schorl_panel::math::Pose;
use schorl_panel::panel::Panel;
use schorl_scope::Background;

/// セッションの持ち方。
///
/// 背景の黒を自分で持つ以上、重ね描き (overlay) ではなくフルセッションになる。
/// 枝が一つなのは `space.background` を満たす形がこれだけだからで、
/// どのランタイムを借りるかとは別の話。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionKind {
    /// 空間そのものを持つセッション。
    Full,
}

/// セッションを開くときの構え。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionConfig {
    /// 持ち方。
    pub kind: SessionKind,
    /// 背景。黒しかない。
    pub background: Background,
    /// 出す板。一枚。
    pub panel: Panel,
}

impl SessionConfig {
    /// pin どおりの構えを作る。背景を引数に取らない。
    pub const fn for_panel(panel: Panel) -> Self {
        Self {
            kind: SessionKind::Full,
            background: Background::Black,
            panel,
        }
    }

    /// 背景を埋める色。
    pub const fn clear_color_rgba(&self) -> [f32; 4] {
        self.background.clear_color_rgba()
    }
}

/// コントローラのボタンの状態。
///
/// [`schorl_input::ButtonState`](../schorl_input/enum.ButtonState.html) とは別物。
/// あちらは Linux へ戻す注入で、こちらは HMD から来る観測。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PressState {
    /// 押した。
    Pressed,
    /// 離した。
    Released,
}

/// セッションの生死。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// 描いてよい。
    Running,
    /// 止まっている (被っていないなど)。
    Idle,
    /// 終わる。
    Stopping,
}

/// ランタイムから上がってくる出来事。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum XrEvent {
    /// セッションの状態が変わった。
    StateChanged(SessionState),
    /// コントローラの姿勢が更新された。
    ControllerPose {
        /// どちらの手か。
        controller: ControllerId,
        /// 世界座標の姿勢。
        pose: Pose,
    },
    /// 板を掴むボタンが動いた。割り当ては `free schorl.grab.button_assignment`。
    GrabButton {
        /// どちらの手か。
        controller: ControllerId,
        /// 押下。
        state: PressState,
    },
    /// カーソルのボタン (クリック) が動いた。
    PointerButton {
        /// どちらの手か。
        controller: ControllerId,
        /// 押下。
        state: PressState,
    },
}

/// 開いているセッション。
///
/// 閉じるのは [`XrSession::end`] か `Drop`。どちらでも閉じる
/// (`house.resource_lifecycle.release_paths`)。
pub trait XrSession: Send {
    /// 溜まった出来事を取り出す。
    fn poll_events(&mut self) -> Result<Vec<XrEvent>>;

    /// 一枚描いて出す。背景は構えの色で埋めること。
    fn submit_frame(&mut self, frame: &Frame, panel: &Panel) -> Result<()>;

    /// 明示的に閉じる。失敗を報告できる経路。
    fn end(&mut self) -> Result<()>;
}

/// セッションを開く capability。
///
/// **実装は後続 phase。** ここに在るのは署名だけ。
pub trait XrRuntime: Send + Sync {
    /// 構えのとおりにセッションを開く。
    fn open_session(&self, config: &SessionConfig) -> Result<Box<dyn XrSession>>;
}

/// 後続 phase が埋める本番の描画ループ。
///
/// 署名だけを先に固定しておく。panic する stub は置かない。
pub trait SessionLoop {
    /// 開いたセッションを回しきる。
    fn run(&mut self, session: &mut dyn XrSession) -> Result<()>;
}

/// ランタイムが使えないときの封筒。実装がまだ無い場所からも使える。
pub fn runtime_unavailable(detail: &str) -> Error {
    Error::new(
        ErrorCode::CapabilityUnavailable,
        "no OpenXR runtime capability is wired in this build",
        TraceId::unattributed(),
    )
    .with_detail("detail", detail)
}

/// 試験用の差し替え実装 (`house.effect_boundary.substitution`)。
pub mod testing {
    use super::*;

    /// 台本どおりの出来事を返し、出した枚数を数えるだけのセッション。
    #[derive(Debug, Default)]
    pub struct ScriptedSession {
        /// 次に返す出来事。
        pub scripted_events: Vec<XrEvent>,
        /// 出した枚数。
        pub submitted_frames: usize,
        /// 閉じた回数。
        pub ended: usize,
    }

    impl XrSession for ScriptedSession {
        fn poll_events(&mut self) -> Result<Vec<XrEvent>> {
            Ok(std::mem::take(&mut self.scripted_events))
        }

        fn submit_frame(&mut self, _frame: &Frame, _panel: &Panel) -> Result<()> {
            self.submitted_frames = self.submitted_frames.saturating_add(1);
            Ok(())
        }

        fn end(&mut self) -> Result<()> {
            self.ended = self.ended.saturating_add(1);
            Ok(())
        }
    }

    /// 台本つきのセッションを一つだけ開く贋物のランタイム。
    #[derive(Debug, Default)]
    pub struct ScriptedRuntime {
        /// 開いたセッションに持たせる出来事。
        pub scripted_events: Vec<XrEvent>,
    }

    impl XrRuntime for ScriptedRuntime {
        fn open_session(&self, _config: &SessionConfig) -> Result<Box<dyn XrSession>> {
            Ok(Box::new(ScriptedSession {
                scripted_events: self.scripted_events.clone(),
                submitted_frames: 0,
                ended: 0,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::ScriptedRuntime;
    use super::*;
    use schorl_capture::{FrameOrigin, PixelFormat};
    use schorl_core::time::UtcTimestamp;

    #[test]
    fn a_session_is_always_black_and_full() {
        let config = SessionConfig::for_panel(Panel::default_single());
        assert_eq!(config.kind, SessionKind::Full);
        assert_eq!(config.background, Background::Black);
        assert_eq!(config.clear_color_rgba(), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(config.panel.count(), 1);
    }

    #[test]
    fn opening_a_scripted_session_yields_its_events() {
        let runtime = ScriptedRuntime {
            scripted_events: vec![XrEvent::StateChanged(SessionState::Running)],
        };
        let config = SessionConfig::for_panel(Panel::default_single());
        let mut session = runtime.open_session(&config).expect("opened");
        assert_eq!(
            session.poll_events().expect("events"),
            vec![XrEvent::StateChanged(SessionState::Running)]
        );
        assert!(
            session.poll_events().expect("events").is_empty(),
            "events are consumed once"
        );
        session.end().expect("ended");
    }

    #[test]
    fn a_frame_can_be_submitted_for_the_single_panel() {
        let runtime = ScriptedRuntime::default();
        let panel = Panel::default_single();
        let mut session = runtime
            .open_session(&SessionConfig::for_panel(panel))
            .expect("opened");
        let frame = Frame::new(
            FrameOrigin::TestDouble,
            PixelFormat::Xrgb8888,
            2,
            2,
            8,
            UtcTimestamp::from_millis_since_epoch(0),
            vec![0; 16],
        )
        .expect("valid frame");
        session.submit_frame(&frame, &panel).expect("submitted");
    }

    #[test]
    fn a_missing_runtime_is_an_envelope_not_a_panic() {
        let err = runtime_unavailable("no active OpenXR runtime json on this host");
        assert_eq!(err.code(), ErrorCode::CapabilityUnavailable);
    }
}
