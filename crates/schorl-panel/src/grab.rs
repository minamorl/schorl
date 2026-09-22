//! 掴んで置き直す。
//!
//! `pin v1.panel_grab: require schorl.v1.panel.reposition = grabbed_by_controller`。
//! 置き直しの入口はコントローラの姿勢だけで、頭の姿勢を受け取る口は無い。
//!
//! どのボタンで掴むかは `free schorl.grab.button_assignment` なので、ここでは
//! 「掴んだ」という事実だけを扱い、束縛の割り当ては上位 (XR 層) に任せる。

use crate::math::Pose;
use crate::panel::PanelPose;

/// どちらの手か。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControllerId {
    /// 左手。
    Left,
    /// 右手。
    Right,
}

/// 掴みの状態。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GrabState {
    /// 離している。板は世界に置かれたまま動かない。
    Released,
    /// 掴んでいる。
    Held {
        /// 掴んでいる手。
        controller: ControllerId,
        /// 掴んだ瞬間の、コントローラ座標系から見た板の姿勢。
        offset: Pose,
    },
}

impl GrabState {
    /// 掴んでいる手。離していれば `None`。
    pub const fn controller(self) -> Option<ControllerId> {
        match self {
            GrabState::Released => None,
            GrabState::Held { controller, .. } => Some(controller),
        }
    }

    /// 掴んでいるか。
    pub const fn is_held(self) -> bool {
        matches!(self, GrabState::Held { .. })
    }
}

/// 掴み始める。掴んだ時点の相対姿勢を覚える。
///
/// 覚えるので、掴んだ瞬間に板が手元へ飛ばない。
pub fn begin_grab(controller: ControllerId, controller_pose: Pose, panel: PanelPose) -> GrabState {
    GrabState::Held {
        controller,
        offset: controller_pose.inverse().compose(panel.pose),
    }
}

/// 掴んでいる間の板の姿勢。離していれば `None`。
///
/// 返るのは常に世界座標の姿勢 ([`crate::panel::PoseFrame::World`])。頭に固定した姿勢は
/// この関数からも出てこない (`ux.panel_not_head_locked`)。
pub fn panel_pose_while_held(grab: GrabState, controller_pose: Pose) -> Option<PanelPose> {
    match grab {
        GrabState::Released => None,
        GrabState::Held { offset, .. } => Some(PanelPose::world(controller_pose.compose(offset))),
    }
}

/// 離す。離した時点の姿勢を板へ書き戻すのは呼ぶ側。
pub const fn release_grab() -> GrabState {
    GrabState::Released
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{Quat, Vec3};
    use crate::panel::PoseFrame;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn close_vec(a: Vec3, b: Vec3) -> bool {
        close(a.x, b.x) && close(a.y, b.y) && close(a.z, b.z)
    }

    fn panel_at(position: Vec3) -> PanelPose {
        PanelPose::world(Pose::new(position, Quat::IDENTITY))
    }

    #[test]
    fn grabbing_without_moving_leaves_the_panel_where_it_was() {
        let controller = Pose::new(Vec3::new(0.2, 1.0, -0.4), Quat::IDENTITY);
        let panel = panel_at(Vec3::new(0.0, 1.3, -1.5));
        let grab = begin_grab(ControllerId::Right, controller, panel);
        let held = panel_pose_while_held(grab, controller).expect("held");
        assert!(close_vec(held.pose.position, panel.pose.position));
    }

    #[test]
    fn moving_the_controller_translates_the_panel_by_the_same_amount() {
        let controller = Pose::new(Vec3::new(0.0, 1.0, -0.4), Quat::IDENTITY);
        let panel = panel_at(Vec3::new(0.0, 1.3, -1.5));
        let grab = begin_grab(ControllerId::Right, controller, panel);

        let moved = Pose::new(Vec3::new(0.5, 1.1, -0.4), Quat::IDENTITY);
        let held = panel_pose_while_held(grab, moved).expect("held");
        assert!(close_vec(held.pose.position, Vec3::new(0.5, 1.4, -1.5)));
        assert_eq!(held.frame, PoseFrame::World);
    }

    #[test]
    fn rotating_the_controller_orbits_the_panel_around_it() {
        let controller = Pose::new(Vec3::ZERO, Quat::IDENTITY);
        let panel = panel_at(Vec3::new(0.0, 0.0, -1.0));
        let grab = begin_grab(ControllerId::Left, controller, panel);

        let turned = Pose::new(
            Vec3::ZERO,
            Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), std::f32::consts::FRAC_PI_2),
        );
        let held = panel_pose_while_held(grab, turned).expect("held");
        assert!(
            close_vec(held.pose.position, Vec3::new(-1.0, 0.0, 0.0)),
            "got {:?}",
            held.pose.position
        );
    }

    #[test]
    fn a_released_grab_yields_no_pose() {
        assert!(panel_pose_while_held(release_grab(), Pose::IDENTITY).is_none());
        assert!(!release_grab().is_held());
        assert_eq!(release_grab().controller(), None);
    }

    #[test]
    fn a_held_grab_remembers_the_hand() {
        let grab = begin_grab(ControllerId::Left, Pose::IDENTITY, panel_at(Vec3::ZERO));
        assert!(grab.is_held());
        assert_eq!(grab.controller(), Some(ControllerId::Left));
    }
}
