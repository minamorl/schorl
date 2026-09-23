//! `XR_MND_headless` で開く本物の OpenXR セッション。
//!
//! なぜ headless か。HMD がこの機に繋がっていない状態で `pin
//! verify.machine_scope` の `openxr_session_opens` を確かめるには、グラフィクス
//! 束縛なしでセッションが開ける経路が要る。一次資料の逐語:
//!
//! > * When this extension is enabled, an application may: call
//! >   flink:xrCreateSession without having made a call to
//! >   ftext:xrGet*GraphicsRequirements, and without an stext:XrGraphicsBinding*
//! >   structure in the pname:next chain of slink:XrSessionCreateInfo.
//! > * In a headless session, the session state should: proceed to
//! >   ename:XR_SESSION_STATE_READY directly from ename:XR_SESSION_STATE_IDLE.
//! > * In a headless session, the
//! >   slink:XrSessionBeginInfo::pname:primaryViewConfigurationType must: be
//! >   ignored and may: be `0`.
//! > * In a headless session, flink:xrEnumerateSwapchainFormats must: return
//! >   ename:XR_SUCCESS but enumerate `0` formats.
//!
//! (OpenXR-Docs `specification/sources/chapters/extensions/mnd/mnd_headless.adoc`)
//!
//! **だから [`HeadlessSession`] は [`XrSession::presents_frames`] が偽である。**
//! 合成面が無いので絵は貼れない。貼れないことを黙って緑にしないための境界であり、
//! 実機での見え方を機械で代用しないという pin (`verify.no_green_substitute`) と
//! 同じ向きに置いてある。掴み・姿勢・クリックの経路はここで本当に動く。
//!
//! 束縛は一次資料の component path をそのまま綴っている
//! (`specification/sources/chapters/semantic_paths.adoc` の
//! Khronos Simple Controller Profile 665-686 行と
//! Oculus Touch Controller Profile 1137-1177 行)。
//! bool の action へ `/value` の scalar を束ねてよいことも逐語で確かめた:
//! 「If the path is to a scalar value, a threshold must: be applied to the value
//! and values over that threshold will be ename:XR_TRUE.」(`input.adoc:512-513`)

use std::sync::Arc;

use openxr::{
    Action, ActionSet, ActiveActionSet, ApplicationInfo, Binding, Entry, Event, EventDataBuffer,
    ExtensionSet, FormFactor, Headless, Instance, Path, Posef, ReferenceSpaceType, Space,
    SpaceLocationFlags, Version, ViewConfigurationType,
};
use schorl_capture::Frame;
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_panel::grab::ControllerId;
use schorl_panel::math::{Pose, Quat, Vec3};
use schorl_panel::panel::Panel;

use crate::composition::{Backdrop, EnvironmentBlend};
use crate::sleep::{Sleeper, ThreadSleeper};
use crate::{PressState, SessionConfig, SessionState, XrEvent, XrRuntime, XrSession};

/// 板を置く座標系の希望。
///
/// どちらも世界固定であり、頭に固定する枝は無い (`ux.panel_not_head_locked`)。
/// `VIEW` は頭に固定される参照空間なので、ここに枝を作っていない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceSpaceChoice {
    /// 部屋の床を基準にした空間。
    Stage,
    /// 起動時の姿勢を基準にした空間。
    Local,
}

impl ReferenceSpaceChoice {
    /// OpenXR の列挙へ写す。
    pub const fn to_openxr(self) -> ReferenceSpaceType {
        match self {
            ReferenceSpaceChoice::Stage => ReferenceSpaceType::STAGE,
            ReferenceSpaceChoice::Local => ReferenceSpaceType::LOCAL,
        }
    }

    /// 報告に出す綴り。
    pub const fn as_str(self) -> &'static str {
        match self {
            ReferenceSpaceChoice::Stage => "stage",
            ReferenceSpaceChoice::Local => "local",
        }
    }
}

/// セッションを開くときの構え。どれも `free` な軸の選択である。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessConfig {
    /// ランタイムへ名乗る名前。
    pub application_name: String,
    /// 欲しい座標系。使えなければもう一方へ落ちる。
    pub preferred_reference_space: ReferenceSpaceChoice,
    /// `READY` を待つ間の一回の眠り (ミリ秒)。
    pub ready_poll_interval_millis: u64,
    /// `READY` を待つ回数の上限。
    pub ready_poll_attempts: u32,
    /// 終了を待つ回数の上限。
    pub shutdown_poll_attempts: u32,
}

impl HeadlessConfig {
    /// v1 の既定。
    pub fn new() -> Self {
        Self {
            application_name: "schorl".to_owned(),
            preferred_reference_space: ReferenceSpaceChoice::Stage,
            ready_poll_interval_millis: 10,
            ready_poll_attempts: 500,
            shutdown_poll_attempts: 200,
        }
    }
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// 開いたセッションについて機械が言い切れる事実。
///
/// 「セッションが開いた」という主張の中身を数字と綴りで持つ。要約ではなく
/// 観測を報告に貼れるようにするために在る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFacts {
    /// ランタイムの名前。
    pub runtime_name: String,
    /// ランタイムの版。
    pub runtime_version: String,
    /// ランタイムが並べた拡張の数。
    pub advertised_extension_count: usize,
    /// `XR_MND_headless` が並んでいたか。
    pub headless_advertised: bool,
    /// 時刻を引ける拡張が在ったか。
    pub timespec_time_advertised: bool,
    /// 実際に作った座標系。
    pub reference_space: ReferenceSpaceChoice,
    /// 束縛を受け取った interaction profile の綴り。
    pub bound_profiles: Vec<String>,
    /// セッションが辿った状態の綴り。
    pub observed_states: Vec<String>,
}

/// `XR_MND_headless` でセッションを開くランタイム。
pub struct HeadlessRuntime {
    config: HeadlessConfig,
    sleeper: Arc<dyn Sleeper>,
}

impl core::fmt::Debug for HeadlessRuntime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HeadlessRuntime")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HeadlessRuntime {
    /// 構えと眠りの実装を与えて作る。
    pub fn new(config: HeadlessConfig, sleeper: Arc<dyn Sleeper>) -> Self {
        Self { config, sleeper }
    }

    /// 本物のスレッドで眠る既定の構え。
    pub fn with_thread_sleeper(config: HeadlessConfig) -> Self {
        Self::new(config, Arc::new(ThreadSleeper))
    }

    /// 構え。
    pub const fn config(&self) -> &HeadlessConfig {
        &self.config
    }

    /// セッションを開き、観測できた事実も一緒に返す。
    ///
    /// [`XrRuntime::open_session`] は `dyn XrSession` しか返せないので、機械検査が
    /// 中身を読めるようにこちらを別に開けてある。
    pub fn open_headless_session(
        &self,
        config: &SessionConfig,
    ) -> Result<(HeadlessSession, RuntimeFacts)> {
        // 0. 構えの黒と、層として出す黒が食い違っていないことを検める。どちらも
        //    型として黒しかないので、これは後から枝が増えたときに落ちる番人である。
        let backdrop = Backdrop::for_background(config.background);
        if backdrop.clear_color_rgba() != config.clear_color_rgba() {
            return Err(Error::new(
                ErrorCode::Internal,
                "the session background and the backdrop layer disagree on the colour",
                TraceId::unattributed(),
            ));
        }

        // 1. loader を実行時に開く。居なければ封筒。
        let entry = unsafe { Entry::load(&()) }.map_err(|e| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "the OpenXR loader could not be opened at run time",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })?;

        // 2. 拡張を数える。headless が無ければ、この経路は成立しない。
        let advertised = entry.enumerate_extensions().map_err(|e| {
            openxr_failure("xrEnumerateInstanceExtensionProperties failed", e)
        })?;
        let advertised_extension_count = count_advertised(&advertised);
        if !advertised.mnd_headless {
            return Err(Error::new(
                ErrorCode::CapabilityUnavailable,
                "the active OpenXR runtime does not advertise XR_MND_headless, so no session can be opened without a graphics binding",
                TraceId::unattributed(),
            )
            .with_detail("advertised_extension_count", advertised_extension_count as i64));
        }

        let mut wanted = ExtensionSet::default();
        wanted.mnd_headless = true;
        // 姿勢を引くには時刻が要る。無ければ姿勢を出さないだけで、セッションは開く。
        wanted.khr_convert_timespec_time = advertised.khr_convert_timespec_time;

        // 3. instance。
        let instance = entry
            .create_instance(
                &ApplicationInfo {
                    application_name: &self.config.application_name,
                    application_version: 1,
                    engine_name: "schorl",
                    engine_version: 1,
                    api_version: Version::new(1, 0, 0),
                },
                &wanted,
                &[],
                &(),
            )
            .map_err(|e| openxr_failure("xrCreateInstance failed", e))?;
        let properties = instance
            .properties()
            .map_err(|e| openxr_failure("xrGetInstanceProperties failed", e))?;

        // 4. system。
        let system = instance
            .system(FormFactor::HEAD_MOUNTED_DISPLAY)
            .map_err(|e| openxr_failure("xrGetSystem failed", e))?;

        // 5. 黒を持てるか。透けるモードしか無いランタイムは断る。
        let offered = instance
            .enumerate_environment_blend_modes(system, ViewConfigurationType::PRIMARY_STEREO)
            .map_err(|e| openxr_failure("xrEnumerateEnvironmentBlendModes failed", e))?;
        let blend = EnvironmentBlend::choose(&offered)?;

        // 6. グラフィクス束縛なしの session。
        let (session, _frame_waiter, _frame_stream) = unsafe {
            instance.create_session::<Headless>(system, &openxr::headless::SessionCreateInfo {})
        }
        .map_err(|e| openxr_failure("xrCreateSession failed for a headless session", e))?;

        let mut observed_states = Vec::new();
        let mut state = SessionState::Idle;

        // 7. READY まで待つ。headless では IDLE から直接 READY へ進む。
        let mut ready = false;
        let mut events = EventDataBuffer::new();
        for _ in 0..self.config.ready_poll_attempts {
            while let Some(event) = instance
                .poll_event(&mut events)
                .map_err(|e| openxr_failure("xrPollEvent failed", e))?
            {
                if let Event::SessionStateChanged(changed) = event {
                    let raw = changed.state();
                    observed_states.push(openxr_state_name(raw).to_owned());
                    state = map_state(raw);
                    if raw == openxr::SessionState::READY {
                        ready = true;
                    }
                }
            }
            if ready {
                break;
            }
            self.sleeper
                .sleep_millis(self.config.ready_poll_interval_millis);
        }
        if !ready {
            return Err(Error::new(
                ErrorCode::HostRefused,
                "the headless session never reached XR_SESSION_STATE_READY",
                TraceId::unattributed(),
            )
            .with_detail("observed_states", observed_states.join(","))
            .with_detail("attempts", i64::from(self.config.ready_poll_attempts)));
        }

        // 8. begin。headless では primaryViewConfigurationType は無視される。
        session
            .begin(ViewConfigurationType::PRIMARY_STEREO)
            .map_err(|e| openxr_failure("xrBeginSession failed", e))?;

        // 9. 座標系。世界固定のものだけを試す。
        let available = session
            .enumerate_reference_spaces()
            .map_err(|e| openxr_failure("xrEnumerateReferenceSpaces failed", e))?;
        let reference_space_choice = pick_reference_space(
            self.config.preferred_reference_space,
            &available,
        )?;
        let reference_space = session
            .create_reference_space(reference_space_choice.to_openxr(), Posef::IDENTITY)
            .map_err(|e| openxr_failure("xrCreateReferenceSpace failed", e))?;

        // 10. 掴み・クリック・手の姿勢。
        let input = HandInput::wire(&instance, &session)?;
        let bound_profiles = input.bound_profiles.clone();

        let facts = RuntimeFacts {
            runtime_name: properties.runtime_name.clone(),
            runtime_version: format!("{}", properties.runtime_version),
            advertised_extension_count,
            headless_advertised: advertised.mnd_headless,
            timespec_time_advertised: advertised.khr_convert_timespec_time,
            reference_space: reference_space_choice,
            bound_profiles,
            observed_states: observed_states.clone(),
        };

        let session = HeadlessSession {
            instance,
            session,
            reference_space,
            input,
            state,
            blend,
            has_time_source: advertised.khr_convert_timespec_time,
            ended: false,
            sleeper: self.sleeper.clone(),
            shutdown_poll_attempts: self.config.shutdown_poll_attempts,
            ready_poll_interval_millis: self.config.ready_poll_interval_millis,
            observed_states,
            last: LastInput::default(),
            opened_for: *config,
        };
        Ok((session, facts))
    }
}

impl XrRuntime for HeadlessRuntime {
    fn open_session(&self, config: &SessionConfig) -> Result<Box<dyn XrSession>> {
        let (session, _facts) = self.open_headless_session(config)?;
        Ok(Box::new(session))
    }
}

/// 押していたかどうかの記憶。押下の変化だけを出来事にするために要る。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LastInput {
    grab: [bool; 2],
    click: [bool; 2],
}

/// 手ごとの束縛。
struct Hand {
    controller: ControllerId,
    subaction_path: Path,
    space: Space,
}

/// 掴み・クリック・姿勢の action 一式。
struct HandInput {
    action_set: ActionSet,
    grab: Action<bool>,
    click: Action<bool>,
    aim: Action<Posef>,
    hands: [Hand; 2],
    bound_profiles: Vec<String>,
}

impl HandInput {
    fn wire(instance: &Instance, session: &openxr::Session<Headless>) -> Result<Self> {
        let action_set = instance
            .create_action_set("schorl", "schorl panel", 0)
            .map_err(|e| openxr_failure("xrCreateActionSet failed", e))?;

        let left = instance
            .string_to_path("/user/hand/left")
            .map_err(|e| openxr_failure("xrStringToPath failed for the left hand", e))?;
        let right = instance
            .string_to_path("/user/hand/right")
            .map_err(|e| openxr_failure("xrStringToPath failed for the right hand", e))?;
        let subactions = [left, right];

        let grab = action_set
            .create_action::<bool>("grab_panel", "grab the panel", &subactions)
            .map_err(|e| openxr_failure("xrCreateAction failed for grab_panel", e))?;
        let click = action_set
            .create_action::<bool>("click_panel", "click on the panel", &subactions)
            .map_err(|e| openxr_failure("xrCreateAction failed for click_panel", e))?;
        let aim = action_set
            .create_action::<Posef>("hand_aim", "where the hand points", &subactions)
            .map_err(|e| openxr_failure("xrCreateAction failed for hand_aim", e))?;

        // 一次資料どおりの component path。profile ごとに綴りが違う。
        let suggestions: [(&str, [&str; 2], [&str; 2], [&str; 2]); 2] = [
            (
                "/interaction_profiles/khr/simple_controller",
                [
                    "/user/hand/left/input/menu/click",
                    "/user/hand/right/input/menu/click",
                ],
                [
                    "/user/hand/left/input/select/click",
                    "/user/hand/right/input/select/click",
                ],
                [
                    "/user/hand/left/input/aim/pose",
                    "/user/hand/right/input/aim/pose",
                ],
            ),
            (
                "/interaction_profiles/oculus/touch_controller",
                [
                    "/user/hand/left/input/squeeze/value",
                    "/user/hand/right/input/squeeze/value",
                ],
                [
                    "/user/hand/left/input/trigger/value",
                    "/user/hand/right/input/trigger/value",
                ],
                [
                    "/user/hand/left/input/aim/pose",
                    "/user/hand/right/input/aim/pose",
                ],
            ),
        ];

        let mut bound_profiles = Vec::new();
        for (profile, grab_paths, click_paths, aim_paths) in suggestions {
            let profile_path = match instance.string_to_path(profile) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let mut bindings = Vec::new();
            let mut ok = true;
            for (index, hand_paths) in [
                (0usize, &grab_paths),
                (1usize, &click_paths),
                (2usize, &aim_paths),
            ] {
                for raw in hand_paths.iter() {
                    match instance.string_to_path(raw) {
                        Ok(path) => bindings.push(match index {
                            0 => Binding::new(&grab, path),
                            1 => Binding::new(&click, path),
                            _ => Binding::new(&aim, path),
                        }),
                        Err(_) => ok = false,
                    }
                }
            }
            if ok
                && instance
                    .suggest_interaction_profile_bindings(profile_path, &bindings)
                    .is_ok()
            {
                bound_profiles.push(profile.to_owned());
            }
        }
        if bound_profiles.is_empty() {
            return Err(Error::new(
                ErrorCode::HostRefused,
                "the runtime accepted no interaction profile bindings, so the panel could not be grabbed",
                TraceId::unattributed(),
            ));
        }

        session
            .attach_action_sets(&[&action_set])
            .map_err(|e| openxr_failure("xrAttachSessionActionSets failed", e))?;

        let left_space = aim
            .create_space(session, left, Posef::IDENTITY)
            .map_err(|e| openxr_failure("xrCreateActionSpace failed for the left hand", e))?;
        let right_space = aim
            .create_space(session, right, Posef::IDENTITY)
            .map_err(|e| openxr_failure("xrCreateActionSpace failed for the right hand", e))?;

        Ok(Self {
            action_set,
            grab,
            click,
            aim,
            hands: [
                Hand {
                    controller: ControllerId::Left,
                    subaction_path: left,
                    space: left_space,
                },
                Hand {
                    controller: ControllerId::Right,
                    subaction_path: right,
                    space: right_space,
                },
            ],
            bound_profiles,
        })
    }
}

/// ランタイムが action へ実際に束ねた入力源の綴り。
///
/// `xrEnumerateBoundSourcesForAction` は action ごとに聞くもので、手ごとには
/// 分かれない。だからこの型も手で割っていない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundSources {
    /// 掴みに束ねられた入力源。
    pub grab: Vec<String>,
    /// クリックに束ねられた入力源。
    pub click: Vec<String>,
    /// 手の姿勢に束ねられた入力源。
    pub aim: Vec<String>,
}

impl BoundSources {
    /// 掴みとクリックと姿勢が、どれも一つ以上の入力源へ束ねられているか。
    pub fn all_bound(&self) -> bool {
        !self.grab.is_empty() && !self.click.is_empty() && !self.aim.is_empty()
    }
}

/// `XR_MND_headless` で開いたセッション。
///
/// 掴み・姿勢・クリックは本当に届く。**絵は貼れない** — 合成面が無いので
/// [`XrSession::presents_frames`] が偽を返す。
pub struct HeadlessSession {
    instance: Instance,
    session: openxr::Session<Headless>,
    reference_space: Space,
    input: HandInput,
    state: SessionState,
    blend: EnvironmentBlend,
    has_time_source: bool,
    ended: bool,
    sleeper: Arc<dyn Sleeper>,
    shutdown_poll_attempts: u32,
    ready_poll_interval_millis: u64,
    observed_states: Vec<String>,
    last: LastInput,
    opened_for: SessionConfig,
}

impl core::fmt::Debug for HeadlessSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HeadlessSession")
            .field("state", &self.state)
            .field("blend", &self.blend)
            .field("has_time_source", &self.has_time_source)
            .field("ended", &self.ended)
            .field("observed_states", &self.observed_states)
            .field("opened_for", &self.opened_for)
            .finish_non_exhaustive()
    }
}

impl HeadlessSession {
    /// 辿った状態の綴り。報告へ生のまま貼るために在る。
    pub fn observed_states(&self) -> &[String] {
        &self.observed_states
    }

    /// 選んだ環境ブレンド。
    pub const fn blend(&self) -> EnvironmentBlend {
        self.blend
    }

    /// どの構えで開いたか。板一枚と黒い背景を持つ構えである。
    pub const fn opened_for(&self) -> &SessionConfig {
        &self.opened_for
    }

    /// ランタイムが実際に束ねた入力源。
    ///
    /// 束縛の提案は「hint」でしかない (`input.adoc:499` の逐語「The bindings
    /// suggested by this system are only a hint to the runtime.」)。提案が通ったことは
    /// 束ねられたことを意味しない。掴みとクリックが本当に入力源へ届いているかは
    /// ここで数えるしかない。
    pub fn bound_sources(&self) -> Result<BoundSources> {
        let grab = self
            .input
            .grab
            .bound_sources(&self.session)
            .map_err(|e| openxr_failure("xrEnumerateBoundSourcesForAction failed for grab", e))?;
        let click = self
            .input
            .click
            .bound_sources(&self.session)
            .map_err(|e| openxr_failure("xrEnumerateBoundSourcesForAction failed for click", e))?;
        let aim = self
            .input
            .aim
            .bound_sources(&self.session)
            .map_err(|e| openxr_failure("xrEnumerateBoundSourcesForAction failed for aim", e))?;
        Ok(BoundSources {
            grab: self.spell_paths(&grab)?,
            click: self.spell_paths(&click)?,
            aim: self.spell_paths(&aim)?,
        })
    }

    fn spell_paths(&self, paths: &[Path]) -> Result<Vec<String>> {
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            out.push(
                self.instance
                    .path_to_string(*path)
                    .map_err(|e| openxr_failure("xrPathToString failed", e))?,
            );
        }
        Ok(out)
    }

    fn drain_runtime_events(&mut self, out: &mut Vec<XrEvent>) -> Result<()> {
        // `XrEventDataBuffer` は生ポインタを持つので `Send` ではない。セッションを
        // スレッド越しに渡せるよう、入れ物は呼び出しごとにスタックへ置く。
        let mut events = EventDataBuffer::new();
        while let Some(event) = self
            .instance
            .poll_event(&mut events)
            .map_err(|e| openxr_failure("xrPollEvent failed", e))?
        {
            if let Event::SessionStateChanged(changed) = event {
                let raw = changed.state();
                self.observed_states.push(openxr_state_name(raw).to_owned());
                let mapped = map_state(raw);
                self.state = mapped;
                out.push(XrEvent::StateChanged(mapped));
            }
        }
        Ok(())
    }

    fn drain_input(&mut self, out: &mut Vec<XrEvent>) -> Result<()> {
        if self.state != SessionState::Running {
            return Ok(());
        }
        self.session
            .sync_actions(&[ActiveActionSet::new(&self.input.action_set)])
            .map_err(|e| openxr_failure("xrSyncActions failed", e))?;

        let time = if self.has_time_source {
            Some(
                self.instance
                    .now()
                    .map_err(|e| openxr_failure("xrConvertTimespecTimeToTimeKHR failed", e))?,
            )
        } else {
            None
        };

        for index in 0..self.input.hands.len() {
            let controller = self.input.hands[index].controller;
            let subaction = self.input.hands[index].subaction_path;

            if let Some(time) = time {
                let location = self.input.hands[index]
                    .space
                    .locate(&self.reference_space, time)
                    .map_err(|e| openxr_failure("xrLocateSpace failed for a hand", e))?;
                if location
                    .location_flags
                    .contains(SpaceLocationFlags::POSITION_VALID | SpaceLocationFlags::ORIENTATION_VALID)
                {
                    out.push(XrEvent::ControllerPose {
                        controller,
                        pose: pose_from_openxr(location.pose),
                    });
                }
            }

            let grab = self
                .input
                .grab
                .state(&self.session, subaction)
                .map_err(|e| openxr_failure("xrGetActionStateBoolean failed for grab", e))?;
            if grab.is_active && grab.current_state != self.last.grab[index] {
                self.last.grab[index] = grab.current_state;
                out.push(XrEvent::GrabButton {
                    controller,
                    state: press_from_bool(grab.current_state),
                });
            }

            let click = self
                .input
                .click
                .state(&self.session, subaction)
                .map_err(|e| openxr_failure("xrGetActionStateBoolean failed for click", e))?;
            if click.is_active && click.current_state != self.last.click[index] {
                self.last.click[index] = click.current_state;
                out.push(XrEvent::PointerButton {
                    controller,
                    state: press_from_bool(click.current_state),
                });
            }
        }
        Ok(())
    }

    fn shut_down(&mut self) -> Result<()> {
        if self.ended {
            return Ok(());
        }
        self.ended = true;
        // 終了を頼む。まだ走っていなければ失敗するので、そこは無視して先へ進む。
        let _ = self.session.request_exit();
        let mut stopping = self.state == SessionState::Stopping;
        let mut events = EventDataBuffer::new();
        for _ in 0..self.shutdown_poll_attempts {
            if stopping {
                break;
            }
            while let Some(event) = self
                .instance
                .poll_event(&mut events)
                .map_err(|e| openxr_failure("xrPollEvent failed during shutdown", e))?
            {
                if let Event::SessionStateChanged(changed) = event {
                    let raw = changed.state();
                    self.observed_states.push(openxr_state_name(raw).to_owned());
                    self.state = map_state(raw);
                    if raw == openxr::SessionState::STOPPING {
                        stopping = true;
                    }
                }
            }
            if !stopping {
                self.sleeper.sleep_millis(self.ready_poll_interval_millis);
            }
        }
        if stopping {
            self.session
                .end()
                .map_err(|e| openxr_failure("xrEndSession failed", e))?;
        }
        Ok(())
    }
}

impl XrSession for HeadlessSession {
    fn poll_events(&mut self) -> Result<Vec<XrEvent>> {
        let mut out = Vec::new();
        self.drain_runtime_events(&mut out)?;
        self.drain_input(&mut out)?;
        Ok(out)
    }

    fn submit_frame(&mut self, _frame: &Frame, _panel: &Panel) -> Result<()> {
        Err(Error::new(
            ErrorCode::Unsupported,
            "a headless session enumerates zero swapchain formats, so no frame can be composited",
            TraceId::unattributed(),
        )
        .with_detail("extension", "XR_MND_headless"))
    }

    fn end(&mut self) -> Result<()> {
        self.shut_down()
    }

    fn presents_frames(&self) -> bool {
        false
    }
}

impl Drop for HeadlessSession {
    fn drop(&mut self) {
        // 成功・失敗・早期 return・巻き戻しのどれでもここを通る
        // (`house.resource_lifecycle.release_paths`)。`Drop` は失敗を返せないので
        // 握り潰す。報告が要るなら [`XrSession::end`] を使う。
        let _ = self.shut_down();
    }
}

/// OpenXR の姿勢を板側の姿勢へ写す。
pub const fn pose_from_openxr(pose: Posef) -> Pose {
    Pose::new(
        Vec3::new(pose.position.x, pose.position.y, pose.position.z),
        Quat::new(
            pose.orientation.x,
            pose.orientation.y,
            pose.orientation.z,
            pose.orientation.w,
        ),
    )
}

/// OpenXR のセッション状態を板側の生死へ写す。
///
/// `VISIBLE` と `FOCUSED` を走行として扱うのは、headless の逐語が
/// 「The VISIBLE and FOCUSED states are only used for their input-related
/// semantics」と述べているため。入力が来る状態が走行である。
pub const fn map_state(state: openxr::SessionState) -> SessionState {
    match state {
        openxr::SessionState::SYNCHRONIZED
        | openxr::SessionState::VISIBLE
        | openxr::SessionState::FOCUSED => SessionState::Running,
        openxr::SessionState::STOPPING
        | openxr::SessionState::LOSS_PENDING
        | openxr::SessionState::EXITING => SessionState::Stopping,
        _ => SessionState::Idle,
    }
}

/// OpenXR のセッション状態の綴り。報告に生のまま貼るために在る。
pub const fn openxr_state_name(state: openxr::SessionState) -> &'static str {
    match state {
        openxr::SessionState::IDLE => "idle",
        openxr::SessionState::READY => "ready",
        openxr::SessionState::SYNCHRONIZED => "synchronized",
        openxr::SessionState::VISIBLE => "visible",
        openxr::SessionState::FOCUSED => "focused",
        openxr::SessionState::STOPPING => "stopping",
        openxr::SessionState::LOSS_PENDING => "loss_pending",
        openxr::SessionState::EXITING => "exiting",
        _ => "unknown",
    }
}

const fn press_from_bool(pressed: bool) -> PressState {
    if pressed {
        PressState::Pressed
    } else {
        PressState::Released
    }
}

/// 世界固定の座標系のうち、使えるものを選ぶ。
///
/// `VIEW` は候補に入れない。頭に固定された空間に板を置くと
/// `ux.panel_not_head_locked` を破るので、選べる枝として持たない。
pub fn pick_reference_space(
    preferred: ReferenceSpaceChoice,
    available: &[ReferenceSpaceType],
) -> Result<ReferenceSpaceChoice> {
    let fallback = match preferred {
        ReferenceSpaceChoice::Stage => ReferenceSpaceChoice::Local,
        ReferenceSpaceChoice::Local => ReferenceSpaceChoice::Stage,
    };
    for choice in [preferred, fallback] {
        if available.contains(&choice.to_openxr()) {
            return Ok(choice);
        }
    }
    Err(Error::new(
        ErrorCode::HostRefused,
        "the runtime offers no world-locked reference space, so the panel could not be left in the world",
        TraceId::unattributed(),
    )
    .with_detail("available_count", available.len() as i64))
}

fn count_advertised(set: &ExtensionSet) -> usize {
    set.names().len()
}

fn openxr_failure(message: &str, error: openxr::sys::Result) -> Error {
    Error::new(ErrorCode::HostRefused, message, TraceId::unattributed())
        .with_detail("openxr_result", i64::from(error.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_world_locked_reference_spaces_can_be_chosen() {
        assert_eq!(
            pick_reference_space(
                ReferenceSpaceChoice::Stage,
                &[ReferenceSpaceType::STAGE, ReferenceSpaceType::LOCAL]
            )
            .expect("stage offered"),
            ReferenceSpaceChoice::Stage
        );
        assert_eq!(
            pick_reference_space(ReferenceSpaceChoice::Stage, &[ReferenceSpaceType::LOCAL])
                .expect("falls back to local"),
            ReferenceSpaceChoice::Local
        );
        assert_eq!(
            pick_reference_space(ReferenceSpaceChoice::Local, &[ReferenceSpaceType::STAGE])
                .expect("falls back to stage"),
            ReferenceSpaceChoice::Stage
        );
        // VIEW だけを出すランタイムは断る。頭に固定された空間に板は置かない。
        let err = pick_reference_space(ReferenceSpaceChoice::Stage, &[ReferenceSpaceType::VIEW])
            .expect_err("view is not world locked");
        assert_eq!(err.code(), ErrorCode::HostRefused);
    }

    #[test]
    fn the_input_bearing_states_are_the_running_ones() {
        assert_eq!(
            map_state(openxr::SessionState::FOCUSED),
            SessionState::Running
        );
        assert_eq!(
            map_state(openxr::SessionState::VISIBLE),
            SessionState::Running
        );
        assert_eq!(
            map_state(openxr::SessionState::SYNCHRONIZED),
            SessionState::Running
        );
        assert_eq!(map_state(openxr::SessionState::IDLE), SessionState::Idle);
        assert_eq!(map_state(openxr::SessionState::READY), SessionState::Idle);
        assert_eq!(
            map_state(openxr::SessionState::STOPPING),
            SessionState::Stopping
        );
        assert_eq!(
            map_state(openxr::SessionState::EXITING),
            SessionState::Stopping
        );
        assert_eq!(
            map_state(openxr::SessionState::LOSS_PENDING),
            SessionState::Stopping
        );
    }

    #[test]
    fn state_names_are_stable_spellings() {
        assert_eq!(openxr_state_name(openxr::SessionState::READY), "ready");
        assert_eq!(openxr_state_name(openxr::SessionState::FOCUSED), "focused");
        assert_eq!(
            openxr_state_name(openxr::SessionState::from_raw(-1)),
            "unknown"
        );
    }

    #[test]
    fn poses_cross_the_boundary_component_by_component() {
        let pose = pose_from_openxr(Posef {
            orientation: openxr::Quaternionf {
                x: 0.1,
                y: 0.2,
                z: 0.3,
                w: 0.9,
            },
            position: openxr::Vector3f {
                x: 1.0,
                y: 2.0,
                z: -3.0,
            },
        });
        assert_eq!(pose.position, Vec3::new(1.0, 2.0, -3.0));
        assert_eq!(pose.orientation, Quat::new(0.1, 0.2, 0.3, 0.9));
    }

    #[test]
    fn a_boolean_press_becomes_the_matching_press_state() {
        assert_eq!(press_from_bool(true), PressState::Pressed);
        assert_eq!(press_from_bool(false), PressState::Released);
    }

    #[test]
    fn the_reference_space_choices_are_world_locked_openxr_spaces() {
        assert_eq!(
            ReferenceSpaceChoice::Stage.to_openxr(),
            ReferenceSpaceType::STAGE
        );
        assert_eq!(
            ReferenceSpaceChoice::Local.to_openxr(),
            ReferenceSpaceType::LOCAL
        );
        assert_eq!(ReferenceSpaceChoice::Stage.as_str(), "stage");
        assert_eq!(ReferenceSpaceChoice::Local.as_str(), "local");
    }

    #[test]
    fn the_default_config_asks_for_a_floor_locked_space() {
        let config = HeadlessConfig::new();
        assert_eq!(
            config.preferred_reference_space,
            ReferenceSpaceChoice::Stage
        );
        assert_eq!(config.application_name, "schorl");
        assert_eq!(config, HeadlessConfig::default());
    }
}
