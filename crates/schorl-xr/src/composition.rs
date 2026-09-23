//! 一フレームぶんの合成計画。**背景の黒をランタイムに任せない。**
//!
//! なぜ層を自分で持つか。OpenXR 1.1 の逐語は OPAQUE をこう定義している:
//!
//! > ename:XR_ENVIRONMENT_BLEND_MODE_OPAQUE.
//! > The composition layers will be displayed with no view of the physical
//! > world behind them.
//! > The composited image will be interpreted as an RGB image, ignoring the
//! > composited alpha channel.
//!
//! (OpenXR-Docs `specification/sources/chapters/rendering.adoc:1564-1568`)
//!
//! 読めるのは「物理世界が透けない」までで、**層に覆われていない領域の色は
//! 約束されていない**。だから `pin space.background` を OPAQUE だけで満たしたと
//! 言うことはできない。schorl は黒を [`Backdrop`] という層として自分で出す。
//!
//! 満たす pin:
//! - `space.background: require schorl.virtual_space.background = black` —
//!   [`Backdrop`] を作る口は [`schorl_scope::Background`] しか受けず、その列挙に
//!   黒以外の枝が無い。計画から背景を外す口も無い。
//! - `v1.scope_excluded` の `passthrough` — [`EnvironmentBlend`] に Opaque 以外の
//!   枝が無い。passthrough を出す ADDITIVE / ALPHA_BLEND は型として存在しない。
//! - `v1.panel_count: require schorl.v1.panel.count = 1` — 板の層は
//!   [`QuadLayer`] 一枚で、層の列を持つ型がここに無い。
//! - `ux.panel_not_head_locked` — quad の座標系は [`PoseFrame::World`] しか取れない。

use openxr::EnvironmentBlendMode;
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::frame::Frame;
use schorl_core::id::TraceId;
use schorl_panel::math::{Pose, Vec3};
use schorl_panel::panel::{Panel, PoseFrame};
use schorl_scope::Background;

use crate::SessionConfig;

/// 合成した絵を環境へどう混ぜるか。
///
/// 枝が一つなのは `v1.scope_excluded` が passthrough を禁じているからで、選択では
/// ない。OpenXR 側の ADDITIVE / ALPHA_BLEND はどちらも物理世界を透かすモードで、
/// ここに対応する枝が無いので組めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnvironmentBlend {
    /// 物理世界を透かさない。
    Opaque,
}

impl EnvironmentBlend {
    /// OpenXR の列挙へ写す。
    pub const fn to_openxr(self) -> EnvironmentBlendMode {
        match self {
            EnvironmentBlend::Opaque => EnvironmentBlendMode::OPAQUE,
        }
    }

    /// ランタイムが申告した一覧から選ぶ。
    ///
    /// OPAQUE が無ければ封筒で拒む。透けるモードで代用して「真っ暗にした」と
    /// 言わないための関門である (`space.background`)。
    pub fn choose(offered: &[EnvironmentBlendMode]) -> Result<Self> {
        if offered.contains(&EnvironmentBlendMode::OPAQUE) {
            return Ok(EnvironmentBlend::Opaque);
        }
        Err(Error::new(
            ErrorCode::HostRefused,
            "the runtime does not offer an opaque environment blend mode, so a black space cannot be owned",
            TraceId::unattributed(),
        )
        .with_detail("offered_modes", offered.len() as i64))
    }
}

/// 視野いっぱいを覆う背景の層。
///
/// quad を一枚出すだけでは背景の黒は保証されない (この module の冒頭の逐語)。
/// この型が計画に必ず一つ入っていることが `space.background` の担保である。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Backdrop {
    background: Background,
}

impl Backdrop {
    /// 背景から作る。[`Background`] に黒以外の枝が無いので、黒以外は組めない。
    pub const fn for_background(background: Background) -> Self {
        Self { background }
    }

    /// 埋める色。
    pub const fn clear_color_rgba(&self) -> [f32; 4] {
        self.background.clear_color_rgba()
    }

    /// 背景。
    pub const fn background(&self) -> Background {
        self.background
    }

    /// 視野いっぱいを覆うか。
    ///
    /// 常に真。覆わない背景層は意味を持たないので、値ではなく型の性質にしてある。
    pub const fn covers_full_view(&self) -> bool {
        true
    }
}

/// 板一枚を貼る平面の層。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuadLayer {
    /// 座標系。世界固定のみ。頭に固定する枝が無い (`ux.panel_not_head_locked`)。
    pub frame: PoseFrame,
    /// 板の中心の世界姿勢。
    pub pose: Pose,
    /// 横幅 (メートル)。
    pub width_m: f32,
    /// 高さ (メートル)。
    pub height_m: f32,
    /// 貼る絵の横の画素数。
    pub source_width_px: u32,
    /// 貼る絵の縦の画素数。
    pub source_height_px: u32,
}

impl QuadLayer {
    /// OpenXR の姿勢へ写す。
    pub const fn pose_openxr(&self) -> openxr::Posef {
        openxr::Posef {
            orientation: openxr::Quaternionf {
                x: self.pose.orientation.x,
                y: self.pose.orientation.y,
                z: self.pose.orientation.z,
                w: self.pose.orientation.w,
            },
            position: openxr::Vector3f {
                x: self.pose.position.x,
                y: self.pose.position.y,
                z: self.pose.position.z,
            },
        }
    }

    /// OpenXR の寸法へ写す。
    pub const fn extent_openxr(&self) -> openxr::Extent2Df {
        openxr::Extent2Df {
            width: self.width_m,
            height: self.height_m,
        }
    }
}

/// 一フレームぶんの合成計画。
///
/// 層は二つ。背景の黒と、板一枚。**層の数と板の枚数は別物である** —
/// `v1.panel_count` が縛るのは後者だけで、黒を出すための層はそこに数えない。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositionPlan {
    /// 環境への混ぜ方。
    pub blend: EnvironmentBlend,
    /// 背景。外せない。
    pub backdrop: Backdrop,
    /// 板。一枚。
    pub panel_quad: QuadLayer,
}

impl CompositionPlan {
    /// 構えと、貼る絵から計画を組む。
    ///
    /// 絵の寸法が板の解像度と食い違っていれば封筒で拒む。板専用の仮想出力は板の
    /// 解像度で注文される (`v1.panel_source`) ので、食い違いは配線の誤りであり、
    /// 黙って引き伸ばすと板上の画素座標がずれる。
    pub fn for_frame(config: &SessionConfig, frame: &Frame) -> Result<Self> {
        let panel = config.panel;
        let resolution = panel.resolution();
        if frame.width_px() != resolution.width_px || frame.height_px() != resolution.height_px {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "captured frame extent does not match the panel resolution",
                TraceId::unattributed(),
            )
            .with_detail("frame_width_px", i64::from(frame.width_px()))
            .with_detail("frame_height_px", i64::from(frame.height_px()))
            .with_detail("panel_width_px", i64::from(resolution.width_px))
            .with_detail("panel_height_px", i64::from(resolution.height_px)));
        }
        Ok(Self::for_panel_unchecked(
            config,
            resolution.width_px,
            resolution.height_px,
        ))
    }

    fn for_panel_unchecked(
        config: &SessionConfig,
        source_width_px: u32,
        source_height_px: u32,
    ) -> Self {
        let panel: Panel = config.panel;
        let pose = panel.pose();
        Self {
            blend: EnvironmentBlend::Opaque,
            backdrop: Backdrop::for_background(config.background),
            panel_quad: QuadLayer {
                frame: pose.frame,
                pose: pose.pose,
                width_m: panel.size().width_m,
                height_m: panel.size().height_m,
                source_width_px,
                source_height_px,
            },
        }
    }

    /// 送る層の数。背景一つと板一つ。
    pub const fn layer_count(&self) -> usize {
        2
    }

    /// 板の層の数。`v1.panel_count` が縛る側。
    pub const fn panel_layer_count(&self) -> usize {
        1
    }

    /// 背景が不透明の黒か。
    pub const fn background_is_opaque_black(&self) -> bool {
        let [r, g, b, a] = self.backdrop.clear_color_rgba();
        r == 0.0 && g == 0.0 && b == 0.0 && a == 1.0
    }

    /// 板の法線 (板が向いている向き)。呼ぶ側が向きを検めるために在る。
    pub const fn panel_normal(&self) -> Vec3 {
        self.panel_quad
            .pose
            .orientation
            .rotate(Vec3::new(0.0, 0.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_core::frame::{FrameOrigin, PixelFormat};
    use schorl_core::time::UtcTimestamp;
    use schorl_panel::math::{Quat, Vec3};
    use schorl_panel::panel::{PanelPose, PanelResolution, PanelSize};

    fn frame_for(width_px: u32, height_px: u32) -> Frame {
        let format = PixelFormat::Xrgb8888;
        let stride = width_px * format.bytes_per_pixel();
        Frame::new(
            FrameOrigin::TestDouble,
            format,
            width_px,
            height_px,
            stride,
            UtcTimestamp::from_millis_since_epoch(0),
            vec![0; (stride as usize) * (height_px as usize)],
        )
        .expect("valid frame")
    }

    fn small_panel() -> Panel {
        Panel::single(
            PanelSize::new(1.2, 0.675).expect("valid size"),
            PanelResolution::new(8, 4).expect("valid resolution"),
            Panel::DEFAULT_POSE,
        )
    }

    #[test]
    fn the_plan_always_carries_an_opaque_black_backdrop() {
        let config = SessionConfig::for_panel(small_panel());
        let plan = CompositionPlan::for_frame(&config, &frame_for(8, 4)).expect("planned");
        assert_eq!(plan.blend, EnvironmentBlend::Opaque);
        assert_eq!(plan.blend.to_openxr(), EnvironmentBlendMode::OPAQUE);
        assert_eq!(plan.backdrop.background(), Background::Black);
        assert!(plan.background_is_opaque_black());
        assert!(plan.backdrop.covers_full_view());
        assert_eq!(plan.backdrop.clear_color_rgba(), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn the_plan_has_one_panel_layer_on_top_of_the_backdrop() {
        let config = SessionConfig::for_panel(small_panel());
        let plan = CompositionPlan::for_frame(&config, &frame_for(8, 4)).expect("planned");
        assert_eq!(plan.panel_layer_count(), 1);
        assert_eq!(plan.layer_count(), 2, "backdrop plus the single panel");
    }

    #[test]
    fn the_quad_is_placed_in_the_world_not_on_the_head() {
        let config = SessionConfig::for_panel(small_panel());
        let plan = CompositionPlan::for_frame(&config, &frame_for(8, 4)).expect("planned");
        assert_eq!(plan.panel_quad.frame, PoseFrame::World);
        assert_eq!(PoseFrame::ALL, [PoseFrame::World]);
    }

    #[test]
    fn the_quad_follows_the_panel_pose_and_size() {
        let moved = small_panel().with_pose(PanelPose::world(Pose::new(
            Vec3::new(0.4, 1.1, -2.0),
            Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), 0.3),
        )));
        let config = SessionConfig::for_panel(moved);
        let plan = CompositionPlan::for_frame(&config, &frame_for(8, 4)).expect("planned");

        assert_eq!(plan.panel_quad.pose, moved.pose().pose);
        assert_eq!(plan.panel_quad.width_m, moved.size().width_m);
        assert_eq!(plan.panel_quad.height_m, moved.size().height_m);

        let openxr_pose = plan.panel_quad.pose_openxr();
        assert_eq!(openxr_pose.position.x, 0.4);
        assert_eq!(openxr_pose.position.y, 1.1);
        assert_eq!(openxr_pose.position.z, -2.0);
        assert_eq!(openxr_pose.orientation.w, moved.pose().pose.orientation.w);

        let extent = plan.panel_quad.extent_openxr();
        assert_eq!(extent.width, moved.size().width_m);
        assert_eq!(extent.height, moved.size().height_m);
    }

    #[test]
    fn a_frame_that_does_not_match_the_panel_resolution_is_refused() {
        let config = SessionConfig::for_panel(small_panel());
        let err = CompositionPlan::for_frame(&config, &frame_for(4, 4)).expect_err("mismatch");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn an_opaque_mode_is_required_and_chosen() {
        assert_eq!(
            EnvironmentBlend::choose(&[EnvironmentBlendMode::OPAQUE]).expect("opaque offered"),
            EnvironmentBlend::Opaque
        );
        assert_eq!(
            EnvironmentBlend::choose(&[
                EnvironmentBlendMode::ADDITIVE,
                EnvironmentBlendMode::OPAQUE
            ])
            .expect("opaque offered among others"),
            EnvironmentBlend::Opaque
        );
        let err = EnvironmentBlend::choose(&[
            EnvironmentBlendMode::ADDITIVE,
            EnvironmentBlendMode::ALPHA_BLEND,
        ])
        .expect_err("only see-through modes");
        assert_eq!(err.code(), ErrorCode::HostRefused);
        let err = EnvironmentBlend::choose(&[]).expect_err("nothing offered");
        assert_eq!(err.code(), ErrorCode::HostRefused);
    }

    #[test]
    fn an_unrotated_panel_faces_along_its_own_normal() {
        let config = SessionConfig::for_panel(small_panel());
        let plan = CompositionPlan::for_frame(&config, &frame_for(8, 4)).expect("planned");
        assert_eq!(plan.panel_normal(), Vec3::new(0.0, 0.0, 1.0));
    }
}
