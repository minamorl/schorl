//! 追跡しているウィンドウと、その置き場所。
//!
//! # 二つの座標系を分けて持つ
//!
//! - **atlas** — Wayland のクライアントへ座標を配るためだけの 2D 平面。
//!   ウィンドウごとに互いに重ならない矩形を割り当てる。smithay の
//!   `PointerHandle::motion` は「合成空間の点」と「その surface の原点」を取り、
//!   差を surface-local として送るので、この平面が要る。
//! - **plane** — 360 度の空間に立っているウィンドウの姿勢。`schorl-panel` の
//!   [`Panel`] をそのまま借りる。平面射影も掴みもあちらが正本である。
//!
//! atlas は宿主の物理モニタとは無関係で、ウィンドウを並べるための内部の目盛りに
//! すぎない。矩形が互いに素なので、atlas の一点は高々一つのウィンドウに属する。
//!
//! # 枚数の上限を置かない
//!
//! `pin v1.window_count: require schorl.v1.window.count = one_or_more` は下限だけを
//! 凍結している。[`WindowRegistry`] に上限の定数は無い。
//! (`schorl_scope::V1_PANEL_COUNT` と [`Panel::count`] は退役した `v1.panel_count`
//!  由来なので、ここでは参照しない。)

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::{Id, TraceId};
use schorl_panel::Panel;
use schorl_panel::cursor::{PixelPoint, to_pixels};
use schorl_panel::math::{Pose, Quat, Vec3};
use schorl_panel::panel::{PanelPose, PanelResolution, PanelSize};

/// 追跡しているウィンドウの識別子。
///
/// 中身は `pin code.id.scheme` が許した方式の [`Id`] だけ。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowId(Id);

impl WindowId {
    /// 発行済みの識別子を包む。
    pub const fn new(id: Id) -> Self {
        Self(id)
    }

    /// 文字列表現。
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// 中の識別子。
    pub const fn id(&self) -> &Id {
        &self.0
    }
}

/// atlas 上の矩形。単位は画素。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasRect {
    /// 左端。
    pub x: i32,
    /// 上端。
    pub y: i32,
    /// 幅。
    pub width_px: i32,
    /// 高さ。
    pub height_px: i32,
}

impl AtlasRect {
    /// 点が矩形の内側か (右端と下端は含まない)。
    pub const fn contains(&self, point: AtlasPoint) -> bool {
        let x = point.x;
        let y = point.y;
        x >= self.x as f64
            && y >= self.y as f64
            && x < (self.x + self.width_px) as f64
            && y < (self.y + self.height_px) as f64
    }

    /// 二つの矩形が重なるか。atlas が互いに素であることを検査するために要る。
    pub const fn overlaps(&self, other: &AtlasRect) -> bool {
        self.x < other.x + other.width_px
            && other.x < self.x + self.width_px
            && self.y < other.y + other.height_px
            && other.y < self.y + self.height_px
    }
}

/// atlas 上の点。画素だが端数を持てる。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtlasPoint {
    /// 左からの画素。
    pub x: f64,
    /// 上からの画素。
    pub y: f64,
}

/// ウィンドウ内の画素座標を atlas の点へ移す。
pub const fn atlas_point_of(rect: AtlasRect, local: PixelPoint) -> AtlasPoint {
    AtlasPoint {
        x: (rect.x + local.x) as f64,
        y: (rect.y + local.y) as f64,
    }
}

/// atlas の点をウィンドウ内の画素座標へ戻す。
pub const fn local_pixel_of(rect: AtlasRect, point: AtlasPoint) -> PixelPoint {
    PixelPoint {
        x: (point.x - rect.x as f64) as i32,
        y: (point.y - rect.y as f64) as i32,
    }
}

/// 開いたウィンドウを 360 度のどこへ置くか。
///
/// `free schorl.window.placement_policy` なので、これは選択であって pin ではない。
/// 目の高さの水平な輪の上へ、決めた角度ずつずらして並べる。0 番は正面 (-Z) に立つ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RingPlacement {
    /// 輪の半径 (メートル)。
    pub radius_m: f32,
    /// 隣へ何ラジアンずらすか。
    pub yaw_step_rad: f32,
    /// 輪の高さ (メートル)。
    pub eye_height_m: f32,
    /// 1 メートルあたり何画素で立てるか。板の既定 (1920px / 1.2m) と同じ密度。
    pub pixels_per_m: f32,
    /// atlas 上でウィンドウ同士の間に空ける画素。
    pub atlas_gutter_px: i32,
}

impl RingPlacement {
    /// v1 の既定。`free schorl.window.placement_policy` の埋め方の一つ。
    pub const DEFAULT: RingPlacement = RingPlacement {
        radius_m: 1.5,
        yaw_step_rad: std::f32::consts::FRAC_PI_6,
        eye_height_m: 1.3,
        pixels_per_m: 1600.0,
        atlas_gutter_px: 64,
    };

    /// `index` 番目のウィンドウの姿勢を決める。
    pub fn plane_for(&self, index: usize, size_px: (i32, i32)) -> Result<Panel> {
        let (width_px, height_px) = size_px;
        let width_px = u32::try_from(width_px).map_err(|_| invalid_size(width_px, height_px))?;
        let height_px =
            u32::try_from(height_px).map_err(|_| invalid_size(width_px as i32, height_px))?;
        let resolution = PanelResolution::new(width_px, height_px)?;
        let size = PanelSize::new(
            width_px as f32 / self.pixels_per_m,
            height_px as f32 / self.pixels_per_m,
        )?;

        let yaw = self.yaw_step_rad * index as f32;
        let position = Vec3::new(
            self.radius_m * yaw.sin(),
            self.eye_height_m,
            -self.radius_m * yaw.cos(),
        );
        let orientation = Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), yaw);
        Ok(Panel::single(
            size,
            resolution,
            PanelPose::world(Pose::new(position, orientation)),
        ))
    }

    /// `index` 番目のウィンドウの atlas 矩形を決める。横一列に間を空けて並べる。
    pub const fn atlas_for(&self, index: usize, size_px: (i32, i32)) -> AtlasRect {
        let (width_px, height_px) = size_px;
        AtlasRect {
            x: (index as i32) * (width_px + self.atlas_gutter_px),
            y: 0,
            width_px,
            height_px,
        }
    }
}

impl Default for RingPlacement {
    fn default() -> Self {
        Self::DEFAULT
    }
}

fn invalid_size(width_px: i32, height_px: i32) -> Error {
    Error::new(
        ErrorCode::InvalidArgument,
        "window size must be positive",
        TraceId::unattributed(),
    )
    .with_detail("width_px", i64::from(width_px))
    .with_detail("height_px", i64::from(height_px))
}

/// 追跡している一枚。
///
/// `handle` は Wayland の toplevel。試験では別の型を入れられるように型引数にしてある
/// (`house.effect_boundary.substitution`)。
#[derive(Debug, Clone)]
pub struct TrackedWindow<H> {
    id: WindowId,
    handle: H,
    rect: AtlasRect,
    plane: Panel,
    mapped: bool,
}

impl<H> TrackedWindow<H> {
    /// 識別子。
    pub const fn id(&self) -> &WindowId {
        &self.id
    }

    /// Wayland 側の取っ手。
    pub const fn handle(&self) -> &H {
        &self.handle
    }

    /// atlas 上の矩形。
    pub const fn rect(&self) -> AtlasRect {
        self.rect
    }

    /// 360 度空間での姿勢。
    pub const fn plane(&self) -> Panel {
        self.plane
    }

    /// 最初のバッファが載ったか。
    pub const fn is_mapped(&self) -> bool {
        self.mapped
    }

    /// 掴んで置き直した結果を書き戻す (`v1.window_grab` の帰り道)。
    ///
    /// atlas はクライアントへ座標を配るための内部の目盛りなので、姿勢を変えても動かさない。
    pub const fn set_plane_pose(&mut self, pose: PanelPose) {
        self.plane = self.plane.with_pose(pose);
    }

    /// バッファが載ったことを記す。
    pub const fn mark_mapped(&mut self) {
        self.mapped = true;
    }

    /// ウィンドウ平面上の点 (メートル) を atlas の点へ移す。
    ///
    /// `ux.cursor_mapping = window_plane_projection` の下流。縁の外の点も
    /// そのまま外側の座標として返る (`ux.cursor_beyond_edge`)。
    pub fn atlas_point(&self, point: schorl_panel::PanelPoint) -> AtlasPoint {
        atlas_point_of(self.rect, to_pixels(&self.plane, point))
    }
}

/// 追跡しているウィンドウの台帳。上限は無い。
#[derive(Debug, Clone)]
pub struct WindowRegistry<H> {
    windows: Vec<TrackedWindow<H>>,
    placement: RingPlacement,
    placed_so_far: usize,
}

impl<H> WindowRegistry<H> {
    /// 置き方を決めて空の台帳を作る。
    pub const fn new(placement: RingPlacement) -> Self {
        Self {
            windows: Vec::new(),
            placement,
            placed_so_far: 0,
        }
    }

    /// 置き方。
    pub const fn placement(&self) -> RingPlacement {
        self.placement
    }

    /// 追跡している枚数。
    pub const fn len(&self) -> usize {
        self.windows.len()
    }

    /// 一枚も無いか。
    pub const fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// 台帳を順に見る。
    pub fn iter(&self) -> impl Iterator<Item = &TrackedWindow<H>> {
        self.windows.iter()
    }

    /// 新しいウィンドウを置く。置いた位置は使い回さない (閉じても番号は進む)。
    pub fn insert(
        &mut self,
        id: WindowId,
        handle: H,
        size_px: (i32, i32),
    ) -> Result<&TrackedWindow<H>> {
        let index = self.placed_so_far;
        let plane = self.placement.plane_for(index, size_px)?;
        let rect = self.placement.atlas_for(index, size_px);
        self.placed_so_far += 1;
        self.windows.push(TrackedWindow {
            id,
            handle,
            rect,
            plane,
            mapped: false,
        });
        self.windows.last().ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "window vanished right after being pushed",
                TraceId::unattributed(),
            )
        })
    }

    /// 識別子で引く。
    pub fn get(&self, id: &WindowId) -> Option<&TrackedWindow<H>> {
        self.windows.iter().find(|w| &w.id == id)
    }

    /// 識別子で引いて書き換える。
    pub fn get_mut(&mut self, id: &WindowId) -> Option<&mut TrackedWindow<H>> {
        self.windows.iter_mut().find(|w| &w.id == id)
    }

    /// 取っ手の条件で引く。
    pub fn find<P: Fn(&H) -> bool>(&self, predicate: P) -> Option<&TrackedWindow<H>> {
        self.windows.iter().find(|w| predicate(&w.handle))
    }

    /// 取っ手の条件で引いて書き換える。
    pub fn find_mut<P: Fn(&H) -> bool>(&mut self, predicate: P) -> Option<&mut TrackedWindow<H>> {
        self.windows.iter_mut().find(|w| predicate(&w.handle))
    }

    /// 取っ手の条件に合うものを外す。外した一枚を返す。
    pub fn remove<P: Fn(&H) -> bool>(&mut self, predicate: P) -> Option<TrackedWindow<H>> {
        let index = self.windows.iter().position(|w| predicate(&w.handle))?;
        Some(self.windows.remove(index))
    }

    /// atlas の一点がどのウィンドウに属するか。
    pub fn window_at(&self, point: AtlasPoint) -> Option<&TrackedWindow<H>> {
        self.windows.iter().find(|w| w.rect.contains(point))
    }

    /// キーボードを向ける既定の相手。最後にバッファが載った一枚。
    ///
    /// `free schorl.window.focus_policy` なので、これも選択である。
    pub fn focus_candidate(&self) -> Option<&TrackedWindow<H>> {
        self.windows.iter().rev().find(|w| w.mapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_core::id::IdScheme;

    fn id(text: &str) -> WindowId {
        WindowId::new(Id::new(IdScheme::Uuidv7, text).expect("non empty"))
    }

    fn registry() -> WindowRegistry<&'static str> {
        WindowRegistry::new(RingPlacement::DEFAULT)
    }

    #[test]
    fn the_registry_has_no_upper_bound_on_windows() {
        let mut reg = registry();
        for n in 0..17 {
            let name: &'static str = Box::leak(format!("w{n}").into_boxed_str());
            reg.insert(id(name), name, (1280, 720)).expect("placed");
        }
        assert_eq!(reg.len(), 17);
    }

    #[test]
    fn atlas_rectangles_never_overlap() {
        let mut reg = registry();
        for n in 0..8 {
            let name: &'static str = Box::leak(format!("w{n}").into_boxed_str());
            reg.insert(id(name), name, (1280, 720)).expect("placed");
        }
        let rects: Vec<AtlasRect> = reg.iter().map(TrackedWindow::rect).collect();
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!a.overlaps(b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_point_inside_a_rectangle_finds_exactly_that_window() {
        let mut reg = registry();
        reg.insert(id("a"), "a", (100, 100)).expect("placed");
        reg.insert(id("b"), "b", (100, 100)).expect("placed");

        let b_rect = reg.get(&id("b")).expect("present").rect();
        let inside = AtlasPoint {
            x: f64::from(b_rect.x) + 3.0,
            y: f64::from(b_rect.y) + 3.0,
        };
        assert_eq!(reg.window_at(inside).map(|w| *w.handle()), Some("b"));

        let gutter = AtlasPoint {
            x: f64::from(b_rect.x) - 1.0,
            y: 3.0,
        };
        assert_eq!(reg.window_at(gutter).map(|w| *w.handle()), None);
    }

    #[test]
    fn the_atlas_point_of_the_plane_centre_is_the_middle_of_the_window() {
        let mut reg = registry();
        reg.insert(id("a"), "a", (1280, 720)).expect("placed");
        let w = reg.get(&id("a")).expect("present");
        let centre = w.atlas_point(schorl_panel::PanelPoint { u_m: 0.0, v_m: 0.0 });
        assert_eq!(
            local_pixel_of(w.rect(), centre),
            PixelPoint { x: 640, y: 360 }
        );
    }

    #[test]
    fn a_point_beyond_the_plane_edge_still_maps_to_a_coordinate() {
        let mut reg = registry();
        reg.insert(id("a"), "a", (1280, 720)).expect("placed");
        let w = reg.get(&id("a")).expect("present");
        // 板の幅は 1280/1600 = 0.8m。半幅 0.4m の外側 0.5m を渡す。
        let outside = w.atlas_point(schorl_panel::PanelPoint { u_m: 0.5, v_m: 0.0 });
        let local = local_pixel_of(w.rect(), outside);
        assert!(local.x > 1280, "{local:?} should be past the right edge");
    }

    #[test]
    fn the_first_window_stands_straight_ahead() {
        let mut reg = registry();
        reg.insert(id("a"), "a", (1280, 720)).expect("placed");
        let pose = reg.get(&id("a")).expect("present").plane().pose().pose;
        assert!((pose.position.x - 0.0).abs() < 1e-6);
        assert!((pose.position.z + 1.5).abs() < 1e-6);
    }

    #[test]
    fn the_second_window_is_turned_away_from_the_first() {
        let mut reg = registry();
        reg.insert(id("a"), "a", (1280, 720)).expect("placed");
        reg.insert(id("b"), "b", (1280, 720)).expect("placed");
        let a = reg
            .get(&id("a"))
            .expect("present")
            .plane()
            .pose()
            .pose
            .position;
        let b = reg
            .get(&id("b"))
            .expect("present")
            .plane()
            .pose()
            .pose
            .position;
        assert!(b.x > a.x, "the ring turns: {a:?} -> {b:?}");
    }

    #[test]
    fn removing_a_window_does_not_reuse_its_place() {
        let mut reg = registry();
        reg.insert(id("a"), "a", (100, 100)).expect("placed");
        let first = reg.get(&id("a")).expect("present").rect();
        reg.remove(|h| *h == "a").expect("removed");
        reg.insert(id("b"), "b", (100, 100)).expect("placed");
        let second = reg.get(&id("b")).expect("present").rect();
        assert_ne!(first, second);
    }

    #[test]
    fn focus_goes_to_the_most_recently_mapped_window() {
        let mut reg = registry();
        reg.insert(id("a"), "a", (100, 100)).expect("placed");
        reg.insert(id("b"), "b", (100, 100)).expect("placed");
        assert!(reg.focus_candidate().is_none(), "nothing is mapped yet");
        reg.get_mut(&id("a")).expect("present").mark_mapped();
        assert_eq!(reg.focus_candidate().map(|w| *w.handle()), Some("a"));
        reg.get_mut(&id("b")).expect("present").mark_mapped();
        assert_eq!(reg.focus_candidate().map(|w| *w.handle()), Some("b"));
    }
}
