//! 描いているセッションから、手の姿勢と掴みボタンを引く面。
//!
//! `schorl-xr` の [`HeadlessRuntime`](schorl_xr::HeadlessRuntime) は同じ action を
//! グラフィクス束縛の無いセッションへ張る。あちらは「ランタイムに入力が在るか」を
//! 測るための経路で、**絵を出さない**。絵を出すセッションは `schorl-render` の
//! `XrVulkanSession` で、そちらには action が張られていない。
//!
//! 掴んで置き直すには、同じセッションの中で「絵」と「手」の両方が要る。
//! その二つを繋ぐのがここで、繋ぎ方は境界の仕事なので
//! (`house.effect_boundary.location = injected_at_boundary`) どちらの crate にも
//! 置いていない。
//!
//! 出来事の語彙は `schorl-xr` の [`XrEvent`] をそのまま使う。掴みの算術は
//! `schorl-panel`、対応の管理は [`crate::grab`]。ここは OpenXR の呼びを
//! [`XrEvent`] へ写すだけで、判断を一つも持たない。
//!
//! # 落ちる順序
//!
//! ここが持つ `ActionSet` / `Action` / `Space` はセッションの子である。
//! **`XrVulkanSession` より先に落とすこと。** 逆にするとランタイムが掴んでいる
//! handle を壊したあとに子を壊す (`house.resource_lifecycle.same_scope`)。

use openxr::{
    Action, ActionSet, ActiveActionSet, Binding, Instance, Path, Posef, Session,
    SpaceLocationFlags, Vulkan,
};
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_panel::grab::ControllerId;
use schorl_render::xr::XrVulkanSession;
use schorl_xr::openxr_runtime::pose_from_openxr;
use schorl_xr::{PressState, XrEvent};

/// 束ねに行く interaction profile と、その component path の綴り。
///
/// 一次資料 (OpenXR 1.0 の "Interaction Profile Paths") の綴りをそのまま写した。
/// `schorl-xr` の headless 側と同じ二本を同じ割り当てで使う。**同じ割り当てで
/// なければ、headless で測った束縛は描く側の証拠にならない。**
const PROFILES: [(&str, [&str; 2], [&str; 2], [&str; 2]); 2] = [
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

struct Hand {
    controller: ControllerId,
    subaction_path: Path,
    space: openxr::Space,
    grab_was_down: bool,
    click_was_down: bool,
}

/// 描いているセッションに張った掴み・クリック・手の姿勢。
pub struct HandInput {
    action_set: ActionSet,
    grab: Action<bool>,
    click: Action<bool>,
    aim: Action<Posef>,
    hands: [Hand; 2],
    bound_profiles: Vec<String>,
    poses_located: u64,
    grab_changes: u64,
    click_changes: u64,
}

impl core::fmt::Debug for HandInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HandInput")
            .field("bound_profiles", &self.bound_profiles)
            .field("poses_located", &self.poses_located)
            .field("grab_changes", &self.grab_changes)
            .field("click_changes", &self.click_changes)
            .finish_non_exhaustive()
    }
}

/// ランタイムが action へ実際に束ねた入力源の綴り。
///
/// 空でないことが「掴みの入口がランタイムに在る」ことの観測である。
/// **在ることと、人が握って押したことは別の主張である。**
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundSources {
    /// 掴みに束ねられた源。
    pub grab: Vec<String>,
    /// クリックに束ねられた源。
    pub click: Vec<String>,
    /// 手の姿勢に束ねられた源。
    pub aim: Vec<String>,
}

impl BoundSources {
    /// 三つとも一本以上束ねられているか。
    pub fn all_bound(&self) -> bool {
        !self.grab.is_empty() && !self.click.is_empty() && !self.aim.is_empty()
    }
}

impl HandInput {
    /// 描いているセッションへ action を張る。
    ///
    /// `xrAttachSessionActionSets` はセッションにつき一度きりなので、この関数も
    /// セッションにつき一度だけ呼ぶこと。
    pub fn wire(session: &XrVulkanSession) -> Result<Self> {
        Self::wire_raw(session.openxr_instance(), session.openxr_session())
    }

    fn wire_raw(instance: &Instance, session: &Session<Vulkan>) -> Result<Self> {
        let action_set = instance
            .create_action_set("schorl", "schorl windows", 0)
            .map_err(|e| failure("xrCreateActionSet failed", e))?;
        let left = instance
            .string_to_path("/user/hand/left")
            .map_err(|e| failure("xrStringToPath failed for the left hand", e))?;
        let right = instance
            .string_to_path("/user/hand/right")
            .map_err(|e| failure("xrStringToPath failed for the right hand", e))?;
        let subactions = [left, right];

        let grab = action_set
            .create_action::<bool>("grab_window", "grab a window", &subactions)
            .map_err(|e| failure("xrCreateAction failed for grab_window", e))?;
        let click = action_set
            .create_action::<bool>("click_window", "click on a window", &subactions)
            .map_err(|e| failure("xrCreateAction failed for click_window", e))?;
        let aim = action_set
            .create_action::<Posef>("hand_aim", "where the hand points", &subactions)
            .map_err(|e| failure("xrCreateAction failed for hand_aim", e))?;

        let mut bound_profiles = Vec::new();
        for (profile, grab_paths, click_paths, aim_paths) in PROFILES {
            let Ok(profile_path) = instance.string_to_path(profile) else {
                continue;
            };
            let mut bindings = Vec::new();
            let mut spelled = true;
            for raw in grab_paths {
                match instance.string_to_path(raw) {
                    Ok(path) => bindings.push(Binding::new(&grab, path)),
                    Err(_) => spelled = false,
                }
            }
            for raw in click_paths {
                match instance.string_to_path(raw) {
                    Ok(path) => bindings.push(Binding::new(&click, path)),
                    Err(_) => spelled = false,
                }
            }
            for raw in aim_paths {
                match instance.string_to_path(raw) {
                    Ok(path) => bindings.push(Binding::new(&aim, path)),
                    Err(_) => spelled = false,
                }
            }
            if spelled
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
                "the runtime accepted no interaction profile bindings, so no window can be grabbed",
                TraceId::unattributed(),
            ));
        }

        session
            .attach_action_sets(&[&action_set])
            .map_err(|e| failure("xrAttachSessionActionSets failed", e))?;

        let left_space = aim
            .create_space(session, left, Posef::IDENTITY)
            .map_err(|e| failure("xrCreateActionSpace failed for the left hand", e))?;
        let right_space = aim
            .create_space(session, right, Posef::IDENTITY)
            .map_err(|e| failure("xrCreateActionSpace failed for the right hand", e))?;

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
                    grab_was_down: false,
                    click_was_down: false,
                },
                Hand {
                    controller: ControllerId::Right,
                    subaction_path: right,
                    space: right_space,
                    grab_was_down: false,
                    click_was_down: false,
                },
            ],
            bound_profiles,
            poses_located: 0,
            grab_changes: 0,
            click_changes: 0,
        })
    }

    /// 束縛を受け取った profile の綴り。
    pub fn bound_profiles(&self) -> &[String] {
        &self.bound_profiles
    }

    /// これまでに姿勢が引けた回数。
    pub const fn poses_located(&self) -> u64 {
        self.poses_located
    }

    /// ランタイムから実際に上がってきた掴みボタンの変化の回数。
    ///
    /// **0 なら、この走りで掴みボタンを押した人は居ない。** 検証側が組み立てた
    /// 出来事と、ランタイムが観測した押下を取り違えないための数である。
    pub const fn grab_changes(&self) -> u64 {
        self.grab_changes
    }

    /// 同じくクリックの変化の回数。
    pub const fn click_changes(&self) -> u64 {
        self.click_changes
    }

    /// ランタイムが action へ実際に束ねた入力源。
    pub fn bound_sources(&self, session: &XrVulkanSession) -> Result<BoundSources> {
        let xr = session.openxr_session();
        let instance = session.openxr_instance();
        let spell = |paths: Vec<Path>| -> Result<Vec<String>> {
            let mut out = Vec::with_capacity(paths.len());
            for path in paths {
                out.push(
                    instance
                        .path_to_string(path)
                        .map_err(|e| failure("xrPathToString failed", e))?,
                );
            }
            Ok(out)
        };
        Ok(BoundSources {
            grab: spell(
                self.grab
                    .bound_sources(xr)
                    .map_err(|e| failure("xrEnumerateBoundSourcesForAction failed for grab", e))?,
            )?,
            click: spell(
                self.click
                    .bound_sources(xr)
                    .map_err(|e| failure("xrEnumerateBoundSourcesForAction failed for click", e))?,
            )?,
            aim: spell(
                self.aim
                    .bound_sources(xr)
                    .map_err(|e| failure("xrEnumerateBoundSourcesForAction failed for aim", e))?,
            )?,
        })
    }

    /// action を同期して、変化を [`XrEvent`] の並びにする。
    ///
    /// 姿勢は直近の `xrWaitFrame` が返した予測表示時刻で引く。まだ一枚も待って
    /// いなければ姿勢は出ない (時刻の無い `xrLocateSpace` は呼べない)。
    pub fn poll(&mut self, session: &XrVulkanSession) -> Result<Vec<XrEvent>> {
        if !session.is_running() {
            return Ok(Vec::new());
        }
        let xr = session.openxr_session();
        xr.sync_actions(&[ActiveActionSet::new(&self.action_set)])
            .map_err(|e| failure("xrSyncActions failed", e))?;

        let mut out = Vec::new();
        let time = session.last_display_time();
        for hand in &mut self.hands {
            if let Some(time) = time {
                let location = hand
                    .space
                    .locate(session.reference_space(), time)
                    .map_err(|e| failure("xrLocateSpace failed for a hand", e))?;
                if location.location_flags.contains(
                    SpaceLocationFlags::POSITION_VALID | SpaceLocationFlags::ORIENTATION_VALID,
                ) {
                    self.poses_located += 1;
                    out.push(XrEvent::ControllerPose {
                        controller: hand.controller,
                        pose: pose_from_openxr(location.pose),
                    });
                }
            }

            let grab = self
                .grab
                .state(xr, hand.subaction_path)
                .map_err(|e| failure("xrGetActionStateBoolean failed for grab", e))?;
            if grab.is_active && grab.current_state != hand.grab_was_down {
                hand.grab_was_down = grab.current_state;
                self.grab_changes += 1;
                out.push(XrEvent::GrabButton {
                    controller: hand.controller,
                    state: press_of(grab.current_state),
                });
            }

            let click = self
                .click
                .state(xr, hand.subaction_path)
                .map_err(|e| failure("xrGetActionStateBoolean failed for click", e))?;
            if click.is_active && click.current_state != hand.click_was_down {
                hand.click_was_down = click.current_state;
                self.click_changes += 1;
                out.push(XrEvent::PointerButton {
                    controller: hand.controller,
                    state: press_of(click.current_state),
                });
            }
        }
        Ok(out)
    }
}

const fn press_of(down: bool) -> PressState {
    if down {
        PressState::Pressed
    } else {
        PressState::Released
    }
}

fn failure(what: &str, error: openxr::sys::Result) -> Error {
    Error::new(ErrorCode::HostRefused, what, TraceId::unattributed())
        .with_detail("openxr_result", format!("{error:?}"))
}
