//! 板の上のカーソル。
//!
//! 満たす pin:
//! - `ux.cursor_mapping: require schorl.cursor.mapping = panel_plane_projection` —
//!   [`project_onto_panel_plane`] は板平面への正射影であり、光線と板の交点ではない。
//!   光線交差を計算する関数はこの crate に無い。
//! - `ux.cursor_beyond_edge: require schorl.cursor.while_grabbed_beyond_panel_edge
//!   = keeps_following` — [`resolve_cursor`] は押している間だけ縁の外でも座標を返す。
//! - `v1.cursor_moves: require schorl.v1.cursor.on_panel = moves` — 板の上では
//!   常に位置が出る。
//!
//! カーソルの見た目は `free schorl.cursor.visual` なので、ここでは位置しか扱わない。

use crate::math::Vec3;
use crate::panel::Panel;

/// 板平面上の位置。板の中心が原点、単位はメートル。
///
/// 縁の外も表せる。表せることが `ux.cursor_beyond_edge` に要る。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelPoint {
    /// 板の右方向の成分。
    pub u_m: f32,
    /// 板の上方向の成分。
    pub v_m: f32,
}

/// 板の画素座標。左上が原点、`y` は下向き。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelPoint {
    /// 左からの画素。負や幅超過もあり得る (縁の外)。
    pub x: i32,
    /// 上からの画素。
    pub y: i32,
}

/// カーソルを押し続けているか。
///
/// ここでの「押している」は板を掴む手ではなく、カーソル自身の引きずり
/// (ボタンを押したままの状態) を指す。板の掴みは [`crate::grab`] が持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerHold {
    /// 離している。
    Up,
    /// 押している。縁を越えても追従する。
    Down,
}

/// カーソルの解決結果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CursorResolution {
    /// 板の上に乗っている。
    OnPanel(PanelPoint),
    /// 縁の外だが、押したままなので追従し続ける。
    BeyondEdgeWhileHeld(PanelPoint),
    /// 縁の外で、押してもいない。板の上のカーソルは動かさない。
    OffPanel,
}

impl CursorResolution {
    /// 追従している位置。`OffPanel` なら `None`。
    pub const fn point(self) -> Option<PanelPoint> {
        match self {
            CursorResolution::OnPanel(p) | CursorResolution::BeyondEdgeWhileHeld(p) => Some(p),
            CursorResolution::OffPanel => None,
        }
    }

    /// 位置が動くか (Linux 側へ動きを届けるか)。
    pub const fn moves_cursor(self) -> bool {
        self.point().is_some()
    }
}

/// 世界座標の一点を板平面へ正射影する。
///
/// 板の向きから右・上の軸を取り、中心からの差をその二軸へ落とす。板までの
/// 距離 (法線方向の成分) は捨てる。これが「レーザーの交点ではなく射影」の意味。
pub fn project_onto_panel_plane(panel: &Panel, point: Vec3) -> PanelPoint {
    let delta = point.sub(panel.pose().pose.position);
    PanelPoint {
        u_m: delta.dot(panel.right_axis()),
        v_m: delta.dot(panel.up_axis()),
    }
}

/// 板平面上の位置が板の内側か。
pub fn is_on_panel(panel: &Panel, point: PanelPoint) -> bool {
    let half_w = panel.size().width_m * 0.5;
    let half_h = panel.size().height_m * 0.5;
    point.u_m.abs() <= half_w && point.v_m.abs() <= half_h
}

/// 射影と押下状態からカーソルの扱いを決める。
pub fn resolve_cursor(panel: &Panel, point: Vec3, hold: PointerHold) -> CursorResolution {
    let projected = project_onto_panel_plane(panel, point);
    match (is_on_panel(panel, projected), hold) {
        (true, _) => CursorResolution::OnPanel(projected),
        (false, PointerHold::Down) => CursorResolution::BeyondEdgeWhileHeld(projected),
        (false, PointerHold::Up) => CursorResolution::OffPanel,
    }
}

/// 板平面上の位置を画素座標へ直す。
///
/// 縁の外の点も、そのまま外側の画素として返す。丸めは切り捨て。
pub fn to_pixels(panel: &Panel, point: PanelPoint) -> PixelPoint {
    let size = panel.size();
    let resolution = panel.resolution();
    let x = (point.u_m + size.width_m * 0.5) / size.width_m * resolution.width_px as f32;
    let y = (size.height_m * 0.5 - point.v_m) / size.height_m * resolution.height_px as f32;
    PixelPoint {
        x: x.floor() as i32,
        y: y.floor() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{Pose, Quat};
    use crate::panel::PanelPose;

    fn panel() -> Panel {
        Panel::default_single().with_pose(PanelPose::world(Pose::new(
            Vec3::new(0.0, 0.0, -1.0),
            Quat::IDENTITY,
        )))
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn projection_drops_the_distance_to_the_plane() {
        // 板から 0.5m 手前の点でも、板平面上の位置は同じになる。
        let near = project_onto_panel_plane(&panel(), Vec3::new(0.3, 0.1, -1.0));
        let far = project_onto_panel_plane(&panel(), Vec3::new(0.3, 0.1, -0.5));
        assert!(close(near.u_m, 0.3) && close(near.v_m, 0.1));
        assert_eq!(near, far);
    }

    #[test]
    fn a_point_over_the_panel_moves_the_cursor() {
        let r = resolve_cursor(&panel(), Vec3::new(0.1, 0.05, -0.2), PointerHold::Up);
        assert!(matches!(r, CursorResolution::OnPanel(_)));
        assert!(r.moves_cursor());
    }

    #[test]
    fn beyond_the_edge_it_keeps_following_only_while_held() {
        let outside = Vec3::new(5.0, 0.0, -1.0);
        let held = resolve_cursor(&panel(), outside, PointerHold::Down);
        assert!(matches!(held, CursorResolution::BeyondEdgeWhileHeld(_)));
        assert!(held.moves_cursor());
        assert!(close(held.point().expect("point").u_m, 5.0));

        let free = resolve_cursor(&panel(), outside, PointerHold::Up);
        assert_eq!(free, CursorResolution::OffPanel);
        assert!(!free.moves_cursor());
    }

    #[test]
    fn pixel_mapping_puts_the_centre_in_the_middle_and_flips_y() {
        let p = panel();
        let centre = to_pixels(&p, PanelPoint { u_m: 0.0, v_m: 0.0 });
        assert_eq!(centre, PixelPoint { x: 960, y: 540 });

        let top_left = to_pixels(
            &p,
            PanelPoint {
                u_m: -p.size().width_m * 0.5,
                v_m: p.size().height_m * 0.5,
            },
        );
        assert_eq!(top_left, PixelPoint { x: 0, y: 0 });
    }

    #[test]
    fn pixels_outside_the_panel_stay_outside() {
        let p = panel();
        let outside = to_pixels(&p, PanelPoint { u_m: 5.0, v_m: 0.0 });
        assert!(
            outside.x > p.resolution().width_px as i32,
            "got {outside:?}"
        );
        let above = to_pixels(&p, PanelPoint { u_m: 0.0, v_m: 5.0 });
        assert!(above.y < 0, "got {above:?}");
    }

    #[test]
    fn the_result_is_the_projection_and_not_where_a_ray_would_hit() {
        // `ux.cursor_mapping` は「レーザーが板に当たった点ではなく板平面への射影」
        // と読める。その二つが別の答えを出す配置で、射影側だけが通ることを押さえる。
        let panel = panel(); // 中心 (0,0,-1)、法線 +Z
        let controller = Vec3::new(0.3, 0.2, -0.4);

        // 板の中心へ向けて構えたコントローラ。レイはその向きに飛ぶ。
        let aim = panel.pose().pose.position.sub(controller);
        // レイと板平面 (z = -1) の交点。
        let t = (panel.pose().pose.position.z - controller.z) / aim.z;
        let ray_hit = controller.add(aim.scale(t));
        let ray_point = PanelPoint {
            u_m: ray_hit.x - panel.pose().pose.position.x,
            v_m: ray_hit.y - panel.pose().pose.position.y,
        };
        assert!(
            close(ray_point.u_m, 0.0) && close(ray_point.v_m, 0.0),
            "the ray aimed at the centre hits the centre: {ray_point:?}"
        );

        let projected = project_onto_panel_plane(&panel, controller);
        assert!(
            close(projected.u_m, 0.3) && close(projected.v_m, 0.2),
            "the projection keeps the hand's own offset: {projected:?}"
        );
        assert_ne!(
            projected, ray_point,
            "a ray-intersection implementation would pass the other assertions too"
        );
    }

    #[test]
    fn the_projection_ignores_where_the_controller_points() {
        // 手の位置が同じなら、どこを向いていても板上の位置は同じ。向きを見ない
        // ことが射影の定義であり、レイとの違いである。
        let panel = panel();
        let hand = Vec3::new(-0.2, 0.15, -0.3);
        let first = project_onto_panel_plane(&panel, hand);
        // 向きを変えても引数に向きが入らないので、答えは動かない。
        let second = project_onto_panel_plane(&panel, hand);
        assert_eq!(first, second);
        assert!(
            close(first.u_m, -0.2) && close(first.v_m, 0.15),
            "{first:?}"
        );
    }

    #[test]
    fn a_rotated_panel_projects_along_its_own_axes() {
        let turned = Panel::default_single().with_pose(PanelPose::world(Pose::new(
            Vec3::ZERO,
            Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), std::f32::consts::FRAC_PI_2),
        )));
        // 板の右方向は回転後 -Z。世界の -Z にある点は板の +u になる。
        let p = project_onto_panel_plane(&turned, Vec3::new(0.0, 0.0, -0.4));
        assert!(close(p.u_m, 0.4), "got {p:?}");
        assert!(close(p.v_m, 0.0));
    }
}
