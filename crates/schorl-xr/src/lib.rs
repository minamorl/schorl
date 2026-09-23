//! `schorl-xr` — 被ったときに見える側。
//!
//! 満たす pin:
//! - `space.background: require schorl.virtual_space.background = black` —
//!   [`SessionConfig`] の背景は [`Background`] なので黒以外を組めない。さらに
//!   [`composition::CompositionPlan`] が黒を**層として**必ず出す。OPAQUE に任せる
//!   だけでは足りない理由は [`composition`] の冒頭に一次資料の逐語で書いてある。
//! - `display.presence` / `display.purpose` — セッションは常に板一枚を伴う。
//! - `v1.panel_grab` — 掴みの入口は [`XrEvent::GrabButton`] と
//!   [`XrEvent::ControllerPose`] だけ。頭の姿勢から板を動かす口は無い。
//! - `verify.machine_scope` の `openxr_session_opens` — セッションを開く操作を
//!   [`XrRuntime::open_session`] という一本の署名にしてあるので、機械検査が
//!   そこだけを突ける。実体は [`openxr_runtime::HeadlessRuntime`]。
//! - `house.resource_lifecycle.*` — [`XrSession`] は明示終了と `Drop` の両方で閉じる。
//!
//! どのランタイムを借りるか (`free schorl.runtime.*` / `free schorl.session.kind`)
//! は pin が自由にしている軸である。この lane は [`SessionKind::Full`] を持つ
//! 自前のセッションとして書き、配信経路には触れていない。
//!
//! **HMD 無しで機械が確かめられるのはここまでである。** 実機を被っての受け入れは
//! [`schorl_verify`](../schorl_verify/index.html) の人間ゲートの仕事で、機械の緑で
//! 代用しない (`verify.no_green_substitute`)。

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::frame::Frame;
use schorl_core::id::TraceId;
use schorl_input::{ButtonState, Keycode};
use schorl_panel::grab::ControllerId;
use schorl_panel::math::Pose;
use schorl_panel::panel::Panel;
use schorl_scope::Background;

pub mod composition;
pub mod openxr_runtime;
pub mod sleep;

pub use composition::{Backdrop, CompositionPlan, EnvironmentBlend, QuadLayer};
pub use openxr_runtime::{
    BoundSources, HeadlessConfig, HeadlessRuntime, HeadlessSession, ReferenceSpaceChoice,
    RuntimeFacts,
};
pub use sleep::{Sleeper, ThreadSleeper};

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

impl PressState {
    /// Linux へ戻す側の押下へ写す。
    ///
    /// 観測と注入は別の型なので、境界のここだけで写す。
    pub const fn to_input_state(self) -> ButtonState {
        match self {
            PressState::Pressed => ButtonState::Pressed,
            PressState::Released => ButtonState::Released,
        }
    }
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
    /// キーが動いた。
    ///
    /// `pin v1.key_delivered` は「キー入力が Linux へ届く」ことだけを縛っており、
    /// **VR 側でキーがどこから来るか (物理キーボードの横取りか、空間に出す板上の
    /// 鍵盤か) は pin されていない。** この枝は外へ渡す経路の入口であり、どの装置が
    /// これを立てるかはまだ決まっていない。
    Key {
        /// どのキーか。綴りの解釈は Linux 側の配列に委ねる。
        keycode: Keycode,
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

    /// このセッションが合成面を持つか。
    ///
    /// 既定は真。`XR_MND_headless` のセッションは偽で、逐語は
    /// 「flink:xrEnumerateSwapchainFormats must: return ename:XR_SUCCESS but
    /// enumerate `0` formats.」— 面が無いので貼れない。
    /// 呼ぶ側はここを見て、貼れないセッションに絵を渡さない。**貼っていないのに
    /// 貼ったと数えないための口である。**
    fn presents_frames(&self) -> bool {
        true
    }
}

/// セッションを開く capability。
pub trait XrRuntime: Send + Sync {
    /// 構えのとおりにセッションを開く。
    fn open_session(&self, config: &SessionConfig) -> Result<Box<dyn XrSession>>;
}

/// 開いたセッションを回しきる口。
///
/// 実体は `schorl-panel-driver` の `PanelDriver` だった。spec 0.2 の原文3 で
/// あの loop が v1 から外れたので、いま v1 の実装はこの trait に無い。
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
    #[derive(Debug)]
    pub struct ScriptedSession {
        /// 次に返す出来事。
        pub scripted_events: Vec<XrEvent>,
        /// 出した枚数。
        pub submitted_frames: usize,
        /// 閉じた回数。
        pub ended: usize,
        /// 合成面を持つことにするか。headless を模すときは偽。
        pub presents: bool,
        /// 何巡目で `Stopping` を混ぜるか。`None` なら混ぜない。
        pub stop_after_polls: Option<usize>,
        /// 取り出しを失敗させるか。解放経路の試験に要る。
        pub poll_error: bool,
        /// これまでの取り出し回数。
        pub polls: usize,
    }

    impl Default for ScriptedSession {
        fn default() -> Self {
            Self {
                scripted_events: Vec::new(),
                submitted_frames: 0,
                ended: 0,
                presents: true,
                stop_after_polls: None,
                poll_error: false,
                polls: 0,
            }
        }
    }

    impl XrSession for ScriptedSession {
        fn poll_events(&mut self) -> Result<Vec<XrEvent>> {
            if self.poll_error {
                return Err(Error::new(
                    ErrorCode::Internal,
                    "the scripted session was asked to fail while polling",
                    TraceId::unattributed(),
                ));
            }
            self.polls = self.polls.saturating_add(1);
            let mut events = std::mem::take(&mut self.scripted_events);
            if let Some(limit) = self.stop_after_polls
                && self.polls >= limit
            {
                events.push(XrEvent::StateChanged(SessionState::Stopping));
            }
            Ok(events)
        }

        fn submit_frame(&mut self, _frame: &Frame, _panel: &Panel) -> Result<()> {
            self.submitted_frames = self.submitted_frames.saturating_add(1);
            Ok(())
        }

        fn end(&mut self) -> Result<()> {
            self.ended = self.ended.saturating_add(1);
            Ok(())
        }

        fn presents_frames(&self) -> bool {
            self.presents
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
                ..Default::default()
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::ScriptedRuntime;
    use super::*;
    use schorl_core::frame::{FrameOrigin, PixelFormat};
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

    #[test]
    fn a_press_maps_onto_the_injection_side_one_to_one() {
        assert_eq!(PressState::Pressed.to_input_state(), ButtonState::Pressed);
        assert_eq!(PressState::Released.to_input_state(), ButtonState::Released);
    }

    #[test]
    fn a_scripted_session_presents_by_default_and_can_be_made_headless() {
        let presenting = super::testing::ScriptedSession::default();
        assert!(presenting.presents_frames());
        let headless = super::testing::ScriptedSession {
            presents: false,
            ..Default::default()
        };
        assert!(!headless.presents_frames());
    }
}
