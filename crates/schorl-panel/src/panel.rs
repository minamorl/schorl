//! 板そのもの。
//!
//! 満たす pin:
//! - `v1.panel_count: require schorl.v1.panel.count = 1` — [`Panel`] を作る口は
//!   [`Panel::single`] だけで、複数枚をまとめて持つ型はこの crate に無い。
//! - `ux.panel_not_head_locked: forbid schorl.panel.pose = head_locked` —
//!   [`PoseFrame`] に頭に固定する枝が無い。姿勢は世界座標でしか表せない。
//! - `display.purpose` / `display.presence` — [`Panel`] は常に作業用の一枚として作る。
//!
//! 板の大きさ・解像度・既定の置き場所は `free schorl.panel.*` なので、ここでの
//! 既定値は選択であって pin ではない。選び直しても pin は壊れない。

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_scope::{DisplayPurpose, V1_PANEL_COUNT};

use crate::math::{Pose, Quat, Vec3};

/// 姿勢をどの座標系で表すか。
///
/// 世界座標しか無い。頭に固定する枝を作らないことが `ux.panel_not_head_locked`
/// の担保であり、散文ではなく型で効く。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PoseFrame {
    /// 世界座標。離しても頭についてこない。
    World,
}

impl PoseFrame {
    /// 表せる座標系のすべて。頭固定がここに増えたら試験が落ちる。
    pub const ALL: [PoseFrame; 1] = [PoseFrame::World];
}

/// 板の物理的な大きさ (メートル)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelSize {
    /// 横幅。
    pub width_m: f32,
    /// 高さ。
    pub height_m: f32,
}

impl PanelSize {
    /// 大きさを作る。正でなければ封筒で拒む。
    pub fn new(width_m: f32, height_m: f32) -> Result<Self> {
        if !(width_m.is_finite() && height_m.is_finite()) || width_m <= 0.0 || height_m <= 0.0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "panel size must be finite and positive",
                TraceId::unattributed(),
            ));
        }
        Ok(Self { width_m, height_m })
    }
}

/// 板が映す画素数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelResolution {
    /// 横の画素数。
    pub width_px: u32,
    /// 縦の画素数。
    pub height_px: u32,
}

impl PanelResolution {
    /// 解像度を作る。0 は封筒で拒む。
    pub fn new(width_px: u32, height_px: u32) -> Result<Self> {
        if width_px == 0 || height_px == 0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "panel resolution must be non-zero in both axes",
                TraceId::unattributed(),
            )
            .with_detail("width_px", i64::from(width_px))
            .with_detail("height_px", i64::from(height_px)));
        }
        Ok(Self {
            width_px,
            height_px,
        })
    }
}

/// 板の姿勢。世界座標の剛体姿勢しか持てない。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelPose {
    /// 座標系。世界固定のみ。
    pub frame: PoseFrame,
    /// 位置と向き。位置は板の中心。
    pub pose: Pose,
}

impl PanelPose {
    /// 世界座標の姿勢から作る。
    pub const fn world(pose: Pose) -> Self {
        Self {
            frame: PoseFrame::World,
            pose,
        }
    }
}

/// 作業用の板一枚。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Panel {
    size: PanelSize,
    resolution: PanelResolution,
    pose: PanelPose,
    purpose: DisplayPurpose,
}

impl Panel {
    /// v1 の既定の大きさ。`free schorl.panel.size` なので選択である。
    pub const DEFAULT_SIZE: PanelSize = PanelSize {
        width_m: 1.2,
        height_m: 0.675,
    };

    /// v1 の既定の解像度。`free schorl.panel.resolution` なので選択である。
    pub const DEFAULT_RESOLUTION: PanelResolution = PanelResolution {
        width_px: 1920,
        height_px: 1080,
    };

    /// v1 の既定の置き場所。目の高さで 1.5m 前。`free schorl.panel.default_pose`。
    pub const DEFAULT_POSE: PanelPose = PanelPose {
        frame: PoseFrame::World,
        pose: Pose {
            position: Vec3 {
                x: 0.0,
                y: 1.3,
                z: -1.5,
            },
            orientation: Quat::IDENTITY,
        },
    };

    /// 唯一の板を作る。枚数を引数に取らないことが `v1.panel_count` の担保。
    pub const fn single(size: PanelSize, resolution: PanelResolution, pose: PanelPose) -> Self {
        Self {
            size,
            resolution,
            pose,
            purpose: DisplayPurpose::Work,
        }
    }

    /// 既定値で作る。
    pub const fn default_single() -> Self {
        Self::single(
            Self::DEFAULT_SIZE,
            Self::DEFAULT_RESOLUTION,
            Self::DEFAULT_POSE,
        )
    }

    /// 大きさ。
    pub const fn size(&self) -> PanelSize {
        self.size
    }

    /// 解像度。
    pub const fn resolution(&self) -> PanelResolution {
        self.resolution
    }

    /// 姿勢。
    pub const fn pose(&self) -> PanelPose {
        self.pose
    }

    /// 用途。作業以外にならない。
    pub const fn purpose(&self) -> DisplayPurpose {
        self.purpose
    }

    /// 置き直す。掴んで離した結果を書き戻す経路。
    pub const fn with_pose(mut self, pose: PanelPose) -> Self {
        self.pose = pose;
        self
    }

    /// 板の枚数。常に 1。
    pub const fn count(&self) -> u8 {
        V1_PANEL_COUNT
    }

    /// 板の右方向 (世界座標の単位ベクトル)。
    pub const fn right_axis(&self) -> Vec3 {
        self.pose.pose.orientation.rotate(Vec3::new(1.0, 0.0, 0.0))
    }

    /// 板の上方向 (世界座標の単位ベクトル)。
    pub const fn up_axis(&self) -> Vec3 {
        self.pose.pose.orientation.rotate(Vec3::new(0.0, 1.0, 0.0))
    }

    /// 板の法線 (表が向く向きの逆、すなわち手前向き)。
    pub const fn normal_axis(&self) -> Vec3 {
        self.pose.pose.orientation.rotate(Vec3::new(0.0, 0.0, 1.0))
    }
}

impl Default for Panel {
    fn default() -> Self {
        Self::default_single()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_only_pose_frame_is_world() {
        assert_eq!(PoseFrame::ALL, [PoseFrame::World]);
        assert_eq!(Panel::default_single().pose().frame, PoseFrame::World);
    }

    #[test]
    fn a_panel_is_always_one_panel_for_work() {
        let panel = Panel::default_single();
        assert_eq!(panel.count(), 1);
        assert_eq!(panel.purpose(), DisplayPurpose::Work);
    }

    #[test]
    fn rejects_degenerate_size_and_resolution() {
        assert_eq!(
            PanelSize::new(0.0, 1.0).expect_err("zero width").code(),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            PanelResolution::new(1920, 0)
                .expect_err("zero height")
                .code(),
            ErrorCode::InvalidArgument
        );
        assert!(PanelSize::new(1.2, 0.675).is_ok());
        assert!(PanelResolution::new(1920, 1080).is_ok());
    }

    #[test]
    fn unrotated_panel_axes_are_the_world_axes() {
        let panel = Panel::default_single();
        assert_eq!(panel.right_axis(), Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(panel.up_axis(), Vec3::new(0.0, 1.0, 0.0));
        assert_eq!(panel.normal_axis(), Vec3::new(0.0, 0.0, 1.0));
    }

    #[test]
    fn repositioning_keeps_the_world_frame() {
        let moved = Panel::default_single().with_pose(PanelPose::world(Pose::new(
            Vec3::new(1.0, 1.0, -2.0),
            Quat::IDENTITY,
        )));
        assert_eq!(moved.pose().frame, PoseFrame::World);
        assert_eq!(moved.pose().pose.position, Vec3::new(1.0, 1.0, -2.0));
    }
}
