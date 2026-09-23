//! コントローラで toplevel を掴んで置き直す継ぎ目。
//!
//! `pin v1.window_grab: require schorl.v1.window.reposition = grabbed_by_controller`
//!
//! # ここが計算しないこと
//!
//! 掴みの算術 ([`begin_grab`] / [`panel_pose_while_held`] / [`release_grab`]) と
//! 平面射影 ([`resolve_cursor`]) は `schorl-panel` が正本である。この module は
//! 一行も計算し直さない。持っているのは「どの手がどのウィンドウを掴んでいるか」
//! という対応だけで、それは `schorl-panel` が持てない (あちらは板一枚の幾何で、
//! ウィンドウの台帳を知らない)。
//!
//! # どのウィンドウを掴むか
//!
//! `free schorl.window.placement_policy` / `free schorl.grab.button_assignment` の
//! どちらにも「掴む相手の選び方」は pin されていない。ここでは
//! [`resolve_cursor`] が `OnPanel` と言うウィンドウのうち、面からの距離が最も
//! 小さい一枚を採る。**選び方そのものは選択であって pin ではない。**

use schorl_compositor::window::WindowId;
use schorl_panel::Panel;
use schorl_panel::cursor::{PointerHold, project_onto_panel_plane, resolve_cursor};
use schorl_panel::grab::{
    ControllerId, GrabState, begin_grab, panel_pose_while_held, release_grab,
};
use schorl_panel::math::{Pose, Vec3};
use schorl_panel::panel::PanelPose;
use schorl_xr::{PressState, XrEvent};

/// 台帳の一枚を、掴みの側から見た形。
///
/// compositor の `TrackedWindow` をそのまま借りると smithay の型が要るので、
/// 掴みに要る二つだけを写した組で受ける。
#[derive(Debug, Clone, PartialEq)]
pub struct WindowPlacement {
    /// どのウィンドウか。
    pub id: WindowId,
    /// いまの面。
    pub plane: Panel,
}

/// 掴みが起こしたこと。
///
/// 姿勢を台帳へ書き戻すのは呼ぶ側である。この module は compositor を触らない。
#[derive(Debug, Clone, PartialEq)]
pub enum GrabEffect {
    /// 掴み始めた。
    Grabbed {
        /// 掴まれたウィンドウ。
        window: WindowId,
        /// 掴んだ手。
        controller: ControllerId,
    },
    /// 掴んだまま動いた。この姿勢を台帳へ書き戻す。
    Moved {
        /// 動いたウィンドウ。
        window: WindowId,
        /// 新しい姿勢。
        pose: PanelPose,
    },
    /// 離した。姿勢はそのまま残る (`ux.window_not_head_locked` の下流)。
    Released {
        /// 離されたウィンドウ。
        window: WindowId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Held {
    controller: ControllerId,
    grab: GrabState,
}

/// どの手がどのウィンドウを掴んでいるかの対応。
#[derive(Debug, Clone, Default)]
pub struct WindowGrabs {
    left_pose: Option<Pose>,
    right_pose: Option<Pose>,
    held: Option<(WindowId, Held)>,
}

impl WindowGrabs {
    /// 何も掴んでいない状態で作る。
    pub const fn new() -> Self {
        Self {
            left_pose: None,
            right_pose: None,
            held: None,
        }
    }

    /// いま掴まれているウィンドウ。
    pub fn held_window(&self) -> Option<&WindowId> {
        self.held.as_ref().map(|(id, _)| id)
    }

    /// その手がいま何かを掴んでいるか。
    pub fn held_by(&self, controller: ControllerId) -> bool {
        self.held
            .as_ref()
            .is_some_and(|(_, held)| held.controller == controller)
    }

    /// その手の最後に観測した姿勢。
    pub const fn controller_pose(&self, controller: ControllerId) -> Option<Pose> {
        match controller {
            ControllerId::Left => self.left_pose,
            ControllerId::Right => self.right_pose,
        }
    }

    const fn remember(&mut self, controller: ControllerId, pose: Pose) {
        match controller {
            ControllerId::Left => self.left_pose = Some(pose),
            ControllerId::Right => self.right_pose = Some(pose),
        }
    }

    /// ランタイムから上がってきた出来事を一つ食べる。
    pub fn on_event(
        &mut self,
        event: &XrEvent,
        placements: &[WindowPlacement],
    ) -> Option<GrabEffect> {
        match *event {
            XrEvent::ControllerPose { controller, pose } => {
                self.remember(controller, pose);
                let (window, held) = self.held.as_ref()?;
                if held.controller != controller {
                    return None;
                }
                let moved = panel_pose_while_held(held.grab, pose)?;
                Some(GrabEffect::Moved {
                    window: window.clone(),
                    pose: moved,
                })
            }
            XrEvent::GrabButton { controller, state } => match state {
                PressState::Pressed => self.begin(controller, placements),
                PressState::Released => self.end(controller),
            },
            _ => None,
        }
    }

    fn begin(
        &mut self,
        controller: ControllerId,
        placements: &[WindowPlacement],
    ) -> Option<GrabEffect> {
        if self.held.is_some() {
            return None;
        }
        let pose = self.controller_pose(controller)?;
        let target = window_under(placements, pose.position)?;
        let grab = begin_grab(controller, pose, target.plane.pose());
        self.held = Some((target.id.clone(), Held { controller, grab }));
        Some(GrabEffect::Grabbed {
            window: target.id.clone(),
            controller,
        })
    }

    fn end(&mut self, controller: ControllerId) -> Option<GrabEffect> {
        let (window, held) = self.held.as_ref()?;
        if held.controller != controller {
            return None;
        }
        let window = window.clone();
        // 離した状態は `schorl-panel` の側の値で表す。ここで枝を作らない。
        let released = release_grab();
        debug_assert!(!released.is_held());
        self.held = None;
        Some(GrabEffect::Released { window })
    }

    /// そのウィンドウが台帳から消えたら掴みも落とす。
    pub fn forget(&mut self, window: &WindowId) {
        if self.held.as_ref().is_some_and(|(id, _)| id == window) {
            self.held = None;
        }
    }
}

/// その一点が面に乗っているウィンドウのうち、面から最も近い一枚。
///
/// 乗っているかどうかは [`resolve_cursor`] に聞く (`ux.cursor_mapping` と
/// 同じ射影を使う)。距離は面の法線方向の成分で測る。
pub fn window_under(placements: &[WindowPlacement], point: Vec3) -> Option<&WindowPlacement> {
    let mut best: Option<(&WindowPlacement, f32)> = None;
    for placement in placements {
        // 押していない状態で聞く。縁の外は掴む相手にならない。
        if resolve_cursor(&placement.plane, point, PointerHold::Up)
            .point()
            .is_none()
        {
            continue;
        }
        let offset = point.sub(placement.plane.pose().pose.position);
        let depth = offset.dot(placement.plane.normal_axis()).abs();
        if best.is_none_or(|(_, d)| depth < d) {
            best = Some((placement, depth));
        }
    }
    best.map(|(placement, _)| placement)
}

/// 面平面へ落とした点。報告に貼るためだけに開けてある。
pub fn projection_on(placement: &WindowPlacement, point: Vec3) -> (f32, f32) {
    let projected = project_onto_panel_plane(&placement.plane, point);
    (projected.u_m, projected.v_m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_core::id::{Id, IdScheme};
    use schorl_panel::math::Quat;
    use schorl_panel::panel::{PanelResolution, PanelSize};

    fn id(text: &str) -> WindowId {
        WindowId::new(Id::new(IdScheme::Uuidv7, text).expect("non empty"))
    }

    fn placement(text: &str, position: Vec3) -> WindowPlacement {
        WindowPlacement {
            id: id(text),
            plane: Panel::single(
                PanelSize::new(0.8, 0.45).expect("positive"),
                PanelResolution::new(1280, 720).expect("non zero"),
                PanelPose::world(Pose::new(position, Quat::IDENTITY)),
            ),
        }
    }

    fn pose_at(position: Vec3) -> Pose {
        Pose::new(position, Quat::IDENTITY)
    }

    #[test]
    fn a_press_without_a_pose_grabs_nothing() {
        let mut grabs = WindowGrabs::new();
        let placements = [placement("a", Vec3::new(0.0, 1.3, -1.5))];
        assert_eq!(
            grabs.on_event(
                &XrEvent::GrabButton {
                    controller: ControllerId::Right,
                    state: PressState::Pressed,
                },
                &placements,
            ),
            None
        );
        assert!(grabs.held_window().is_none());
    }

    #[test]
    fn pointing_at_a_window_and_pressing_grabs_exactly_that_window() {
        let mut grabs = WindowGrabs::new();
        let placements = [
            placement("a", Vec3::new(0.0, 1.3, -1.5)),
            placement("b", Vec3::new(3.0, 1.3, -1.5)),
        ];
        grabs.on_event(
            &XrEvent::ControllerPose {
                controller: ControllerId::Right,
                pose: pose_at(Vec3::new(0.0, 1.3, -1.0)),
            },
            &placements,
        );
        let effect = grabs
            .on_event(
                &XrEvent::GrabButton {
                    controller: ControllerId::Right,
                    state: PressState::Pressed,
                },
                &placements,
            )
            .expect("grabbed");
        assert_eq!(
            effect,
            GrabEffect::Grabbed {
                window: id("a"),
                controller: ControllerId::Right,
            }
        );
        assert_eq!(grabs.held_window(), Some(&id("a")));
    }

    #[test]
    fn moving_the_controller_while_held_moves_the_window_by_the_same_amount() {
        let mut grabs = WindowGrabs::new();
        let placements = [placement("a", Vec3::new(0.0, 1.3, -1.5))];
        grabs.on_event(
            &XrEvent::ControllerPose {
                controller: ControllerId::Right,
                pose: pose_at(Vec3::new(0.0, 1.3, -1.0)),
            },
            &placements,
        );
        grabs.on_event(
            &XrEvent::GrabButton {
                controller: ControllerId::Right,
                state: PressState::Pressed,
            },
            &placements,
        );
        let effect = grabs
            .on_event(
                &XrEvent::ControllerPose {
                    controller: ControllerId::Right,
                    pose: pose_at(Vec3::new(0.4, 1.5, -1.0)),
                },
                &placements,
            )
            .expect("moved");
        match effect {
            GrabEffect::Moved { window, pose } => {
                assert_eq!(window, id("a"));
                assert!((pose.pose.position.x - 0.4).abs() < 1e-5, "{pose:?}");
                assert!((pose.pose.position.y - 1.5).abs() < 1e-5, "{pose:?}");
                assert!((pose.pose.position.z + 1.5).abs() < 1e-5, "{pose:?}");
            }
            other => panic!("expected a move, got {other:?}"),
        }
    }

    #[test]
    fn a_pose_from_the_other_hand_does_not_move_the_held_window() {
        let mut grabs = WindowGrabs::new();
        let placements = [placement("a", Vec3::new(0.0, 1.3, -1.5))];
        grabs.on_event(
            &XrEvent::ControllerPose {
                controller: ControllerId::Right,
                pose: pose_at(Vec3::new(0.0, 1.3, -1.0)),
            },
            &placements,
        );
        grabs.on_event(
            &XrEvent::GrabButton {
                controller: ControllerId::Right,
                state: PressState::Pressed,
            },
            &placements,
        );
        assert_eq!(
            grabs.on_event(
                &XrEvent::ControllerPose {
                    controller: ControllerId::Left,
                    pose: pose_at(Vec3::new(9.0, 9.0, 9.0)),
                },
                &placements,
            ),
            None
        );
    }

    #[test]
    fn releasing_ends_the_grab_and_further_motion_does_nothing() {
        let mut grabs = WindowGrabs::new();
        let placements = [placement("a", Vec3::new(0.0, 1.3, -1.5))];
        grabs.on_event(
            &XrEvent::ControllerPose {
                controller: ControllerId::Right,
                pose: pose_at(Vec3::new(0.0, 1.3, -1.0)),
            },
            &placements,
        );
        grabs.on_event(
            &XrEvent::GrabButton {
                controller: ControllerId::Right,
                state: PressState::Pressed,
            },
            &placements,
        );
        assert_eq!(
            grabs.on_event(
                &XrEvent::GrabButton {
                    controller: ControllerId::Right,
                    state: PressState::Released,
                },
                &placements,
            ),
            Some(GrabEffect::Released { window: id("a") })
        );
        assert!(grabs.held_window().is_none());
        assert_eq!(
            grabs.on_event(
                &XrEvent::ControllerPose {
                    controller: ControllerId::Right,
                    pose: pose_at(Vec3::new(5.0, 5.0, 5.0)),
                },
                &placements,
            ),
            None
        );
    }

    #[test]
    fn a_point_off_every_plane_grabs_nothing() {
        let placements = [placement("a", Vec3::new(0.0, 1.3, -1.5))];
        assert!(window_under(&placements, Vec3::new(4.0, 1.3, -1.0)).is_none());
    }

    #[test]
    fn the_nearer_plane_wins_when_two_overlap_in_projection() {
        let placements = [
            placement("far", Vec3::new(0.0, 1.3, -3.0)),
            placement("near", Vec3::new(0.0, 1.3, -1.6)),
        ];
        let chosen = window_under(&placements, Vec3::new(0.0, 1.3, -1.5)).expect("one of them");
        assert_eq!(chosen.id, id("near"));
    }

    #[test]
    fn forgetting_a_vanished_window_drops_the_grab() {
        let mut grabs = WindowGrabs::new();
        let placements = [placement("a", Vec3::new(0.0, 1.3, -1.5))];
        grabs.on_event(
            &XrEvent::ControllerPose {
                controller: ControllerId::Right,
                pose: pose_at(Vec3::new(0.0, 1.3, -1.0)),
            },
            &placements,
        );
        grabs.on_event(
            &XrEvent::GrabButton {
                controller: ControllerId::Right,
                state: PressState::Pressed,
            },
            &placements,
        );
        grabs.forget(&id("a"));
        assert!(grabs.held_window().is_none());
    }
}
