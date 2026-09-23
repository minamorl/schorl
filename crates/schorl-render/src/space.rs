//! 空間の側: 黒い背景と、そこへ置く矩形のサーフェス、そして view の行列。
//!
//! # 黒を自分で持つということ
//!
//! `pin space.background: require schorl.virtual_space.background = black` は
//! 「背景が黒い」という要求である。ランタイム既定の void に任せると、その黒は
//! ランタイムの持ち物になる。[`Renderer`](crate::Renderer) は毎フレーム swapchain の
//! 全画素を [`Background::clear_color_rgba`] で clear するので、出る黒は
//! こちらが書いた画素である。
//!
//! `pin space.extent: require schorl.virtual_space.extent = 360_degrees` は
//! 「360 度の空間」である。持つ層は projection 層一種だけで、これは view の
//! 錐台を丸ごと埋める。頭がどちらを向いても自分が clear した黒が出る。
//! 方角ごとに層を差し替える枝は無い。
//!
//! # 射影行列の出どころ
//!
//! `XrFovf` から射影行列を組む式は OpenXR-SDK の `src/common/xr_linear.h` の
//! `XrMatrix4x4f_CreateProjection` の逐語である。Vulkan の clip space について
//! 同ファイルはこう書いている:
//!
//! > Set to tanAngleDown - tanAngleUp for a clip space with positive Y down (Vulkan).
//! > Set to tanAngleUp - tanAngleDown for a clip space with positive Y up (OpenGL / D3D / Metal).
//!
//! > Set to nearZ for a \[-1,1\] Z clip space (OpenGL / OpenGL ES).
//! > Set to zero for a \[0,1\] Z clip space (Vulkan / D3D / Metal).
//!
//! よって Vulkan では `tanAngleHeight = tanDown - tanUp`、`offsetZ = 0` を採る。
//! `m[i]` は列優先で `m[col * 4 + row]` である。

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_panel::math::{Pose, Vec3};
use schorl_scope::Background;

use crate::texture::Texture;

/// 4x4 行列。列優先で持つ (Vulkan / GLSL の `mat4` と同じ並び)。
///
/// `columns[c][r]` が `xr_linear.h` の `m[c * 4 + r]` にあたる。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat4 {
    /// 列。
    pub columns: [[f32; 4]; 4],
}

impl Mat4 {
    /// 単位行列。
    pub const IDENTITY: Mat4 = Mat4 {
        columns: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ],
    };

    /// `self * other`。
    ///
    /// `std::ops::Mul` にしていないのは、行列の積は可換でないので `a * b` の
    /// 見た目より `a.mul(b)` の方が向きを間違えにくいためである。
    #[allow(clippy::should_implement_trait, reason = "積の向きを名前で示したい")]
    pub fn mul(self, other: Mat4) -> Mat4 {
        let mut columns = [[0.0f32; 4]; 4];
        for (c, column) in columns.iter_mut().enumerate() {
            for (r, cell) in column.iter_mut().enumerate() {
                let mut sum = 0.0;
                for k in 0..4 {
                    sum += self.columns[k][r] * other.columns[c][k];
                }
                *cell = sum;
            }
        }
        Mat4 { columns }
    }

    /// push constant へそのまま置ける平らな 16 個。
    pub fn to_array(self) -> [f32; 16] {
        let mut out = [0.0f32; 16];
        for (c, column) in self.columns.iter().enumerate() {
            for (r, cell) in column.iter().enumerate() {
                out[c * 4 + r] = *cell;
            }
        }
        out
    }

    /// 姿勢 (回転 + 平行移動) と、x/y 方向の拡大から model 行列を組む。
    ///
    /// ローカル平面は x が右、y が上、z = 0 で、四隅は ±0.5。よって
    /// `scale_x` / `scale_y` にサーフェスの幅・高さ (メートル) を渡すと
    /// そのままの寸法になる。
    pub fn from_pose_scaled(pose: Pose, scale_x: f32, scale_y: f32) -> Mat4 {
        let right = pose.orientation.rotate(Vec3::new(scale_x, 0.0, 0.0));
        let up = pose.orientation.rotate(Vec3::new(0.0, scale_y, 0.0));
        let normal = pose.orientation.rotate(Vec3::new(0.0, 0.0, 1.0));
        Mat4 {
            columns: [
                [right.x, right.y, right.z, 0.0],
                [up.x, up.y, up.z, 0.0],
                [normal.x, normal.y, normal.z, 0.0],
                [pose.position.x, pose.position.y, pose.position.z, 1.0],
            ],
        }
    }

    /// 姿勢の逆を view 行列として組む。
    ///
    /// 目の姿勢は「世界の中で目がどこを向いているか」なので、世界を目の座標系へ
    /// 引き戻すには逆を掛ける。[`Pose::inverse`] を使うので、回転の扱いは
    /// `schorl-panel` と同じ一箇所に寄せてある。
    pub fn view_from_pose(pose: Pose) -> Mat4 {
        let inverse = pose.inverse();
        let right = inverse.orientation.rotate(Vec3::new(1.0, 0.0, 0.0));
        let up = inverse.orientation.rotate(Vec3::new(0.0, 1.0, 0.0));
        let back = inverse.orientation.rotate(Vec3::new(0.0, 0.0, 1.0));
        Mat4 {
            columns: [
                [right.x, right.y, right.z, 0.0],
                [up.x, up.y, up.z, 0.0],
                [back.x, back.y, back.z, 0.0],
                [
                    inverse.position.x,
                    inverse.position.y,
                    inverse.position.z,
                    1.0,
                ],
            ],
        }
    }
}

/// 片目の view と射影。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewProjection {
    /// view 行列。
    pub view: Mat4,
    /// 射影行列。
    pub projection: Mat4,
}

impl ViewProjection {
    /// OpenXR の `View` (姿勢 + 視野) から組む。
    ///
    /// `near_z` は 0 より大きいこと。`far_z <= near_z` のときは
    /// `xr_linear.h` の逐語どおり無限遠の射影になる。
    pub fn from_openxr(view: openxr::View, near_z: f32, far_z: f32) -> Result<Self> {
        // NaN も弾く。`>` の否定ではなく、比較できない値を明示的に落とす形で書く。
        if !near_z.is_finite() || near_z <= 0.0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "the near plane distance must be greater than zero",
                TraceId::unattributed(),
            ));
        }
        let pose = schorl_xr::openxr_runtime::pose_from_openxr(view.pose);
        Ok(Self {
            view: Mat4::view_from_pose(pose),
            projection: projection_from_fov(view.fov, near_z, far_z),
        })
    }

    /// `projection * view * model`。
    pub fn view_projection(self) -> Mat4 {
        self.projection.mul(self.view)
    }
}

/// `XrFovf` から Vulkan 向けの射影行列を組む。
///
/// 出どころは `xr_linear.h` の `XrMatrix4x4f_CreateProjection` の逐語で、
/// `graphicsApi == GRAPHICS_VULKAN` の枝を採る。
pub fn projection_from_fov(fov: openxr::Fovf, near_z: f32, far_z: f32) -> Mat4 {
    let tan_left = fov.angle_left.tan();
    let tan_right = fov.angle_right.tan();
    let tan_up = fov.angle_up.tan();
    let tan_down = fov.angle_down.tan();

    let tan_angle_width = tan_right - tan_left;
    // 逐語: 「Set to tanAngleDown - tanAngleUp for a clip space with positive Y down (Vulkan).」
    let tan_angle_height = tan_down - tan_up;
    // 逐語: 「Set to zero for a [0,1] Z clip space (Vulkan / D3D / Metal).」
    let offset_z = 0.0f32;

    let (m10, m14) = if far_z <= near_z {
        // 逐語: 「The far plane is placed at infinity if farZ <= nearZ.」
        (-1.0, -(near_z + offset_z))
    } else {
        (
            -(far_z + offset_z) / (far_z - near_z),
            -(far_z * (near_z + offset_z)) / (far_z - near_z),
        )
    };

    Mat4 {
        columns: [
            [2.0 / tan_angle_width, 0.0, 0.0, 0.0],
            [0.0, 2.0 / tan_angle_height, 0.0, 0.0],
            [
                (tan_right + tan_left) / tan_angle_width,
                (tan_up + tan_down) / tan_angle_height,
                m10,
                -1.0,
            ],
            [0.0, 0.0, m14, 0.0],
        ],
    }
}

/// サーフェスの実寸 (メートル)。
///
/// `free schorl.window.size` は pin されていない軸なので、値の決め方は自由である。
/// 0 以下を作れないことだけを型で守る。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceSize {
    width_m: f32,
    height_m: f32,
}

impl SurfaceSize {
    /// 正の寸法から作る。0 以下と非数は封筒で拒む。
    pub fn new(width_m: f32, height_m: f32) -> Result<Self> {
        // NaN も弾く。schorl_panel::PanelSize::new と同じ形にそろえてある。
        if !(width_m.is_finite() && height_m.is_finite()) || width_m <= 0.0 || height_m <= 0.0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "a surface must have a positive extent in metres",
                TraceId::unattributed(),
            ));
        }
        Ok(Self { width_m, height_m })
    }

    /// 幅 (メートル)。
    pub const fn width_m(self) -> f32 {
        self.width_m
    }

    /// 高さ (メートル)。
    pub const fn height_m(self) -> f32 {
        self.height_m
    }
}

/// 空間に置かれた矩形のサーフェス一枚。
///
/// `pin space.content_unit = window` の「ウィンドウ」に相当する描画の単位である。
/// **曲げる媒介変数が無い** のは `pin v1.scope_excluded` が `curved_surface` を
/// 禁じているため。
///
/// `pose` は参照空間 (STAGE / LOCAL) の姿勢である。view の姿勢から引く口が無いので
/// `forbid schorl.window.pose = head_locked` を型で守っている。
#[derive(Debug)]
pub struct Surface {
    /// 参照空間での姿勢。
    pub pose: Pose,
    /// 実寸。
    pub size: SurfaceSize,
    /// 貼るテクスチャ。
    pub texture: Texture,
}

/// 一枚を描くために renderer が要る最小の組。
///
/// [`Surface`] はテクスチャを所有するが、描く側は借りれば足りる。読み戻しの
/// ように所有していないテクスチャを一枚だけ描きたい場面があるので、描画の口は
/// 所有ではなく借用で受ける。
#[derive(Debug, Clone, Copy)]
pub struct SurfaceDraw<'a> {
    /// 貼るテクスチャ。
    pub texture: &'a Texture,
    /// ローカル平面 (±0.5) から参照空間への行列。
    pub model: Mat4,
}

impl Surface {
    /// 姿勢・寸法・テクスチャから作る。
    pub const fn new(pose: Pose, size: SurfaceSize, texture: Texture) -> Self {
        Self {
            pose,
            size,
            texture,
        }
    }

    /// このサーフェスの model 行列。
    pub fn model(&self) -> Mat4 {
        Mat4::from_pose_scaled(self.pose, self.size.width_m(), self.size.height_m())
    }

    /// 描く側へ渡す借用の組。
    pub fn as_draw(&self) -> SurfaceDraw<'_> {
        SurfaceDraw {
            texture: &self.texture,
            model: self.model(),
        }
    }
}

/// 背景の色を Vulkan の clear 値へ。
///
/// [`Background`] は黒しか持たないので、ここも一枝しかない。
pub const fn clear_color(background: Background) -> [f32; 4] {
    background.clear_color_rgba()
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_panel::math::Quat;

    fn fov(l: f32, r: f32, u: f32, d: f32) -> openxr::Fovf {
        openxr::Fovf {
            angle_left: l,
            angle_right: r,
            angle_up: u,
            angle_down: d,
        }
    }

    #[test]
    fn the_background_clear_value_is_opaque_black() {
        // pin space.background。透けない黒であること (alpha = 1)。
        assert_eq!(clear_color(Background::Black), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn the_projection_matches_xr_linear_h_for_a_symmetric_fov() {
        // xr_linear.h の逐語を手計算で照合する。左右上下が対称なら
        // m[8] と m[9] は 0、m[0] = 2 / (tanR - tanL)。
        let half = core::f32::consts::FRAC_PI_4;
        let p = projection_from_fov(fov(-half, half, half, -half), 0.05, 100.0);
        let t = half.tan();
        assert!((p.columns[0][0] - 2.0 / (2.0 * t)).abs() < 1e-6);
        assert!(p.columns[2][0].abs() < 1e-6);
        assert!(p.columns[2][1].abs() < 1e-6);
        // Vulkan の枝: tanAngleHeight = tanDown - tanUp なので負になり、y が反転する。
        assert!(p.columns[1][1] < 0.0, "Vulkan clip space has +Y down");
        // 逐語: m[11] = -1
        assert_eq!(p.columns[2][3], -1.0);
        assert_eq!(p.columns[3][3], 0.0);
    }

    #[test]
    fn an_infinite_far_plane_follows_the_verbatim_branch() {
        // 逐語: 「The far plane is placed at infinity if farZ <= nearZ.」
        let half = core::f32::consts::FRAC_PI_4;
        let near = 0.05f32;
        let p = projection_from_fov(fov(-half, half, half, -half), near, near);
        assert_eq!(p.columns[2][2], -1.0);
        assert!((p.columns[3][2] - -near).abs() < 1e-7);
    }

    #[test]
    fn a_point_in_front_of_the_eye_lands_inside_the_clip_volume() {
        // 目の前 1m の点が w > 0 で、正規化後に [-1,1]x[-1,1]x[0,1] に入ること。
        let half = core::f32::consts::FRAC_PI_4;
        let vp = ViewProjection {
            view: Mat4::view_from_pose(Pose::IDENTITY),
            projection: projection_from_fov(fov(-half, half, half, -half), 0.05, 100.0),
        };
        let m = vp.view_projection();
        // OpenXR の慣習では -Z が前。
        let p = [0.0f32, 0.0, -1.0, 1.0];
        let mut out = [0.0f32; 4];
        for (r, cell) in out.iter_mut().enumerate() {
            *cell = (0..4).map(|k| m.columns[k][r] * p[k]).sum();
        }
        assert!(out[3] > 0.0, "the point must be in front of the eye");
        let z = out[2] / out[3];
        assert!((0.0..=1.0).contains(&z), "z = {z} outside the Vulkan range");
        let x = out[0] / out[3];
        let y = out[1] / out[3];
        assert!(
            x.abs() < 1e-5 && y.abs() < 1e-5,
            "the centre must stay centred"
        );
    }

    #[test]
    fn a_point_behind_the_eye_is_outside_the_clip_volume() {
        let half = core::f32::consts::FRAC_PI_4;
        let vp = ViewProjection {
            view: Mat4::view_from_pose(Pose::IDENTITY),
            projection: projection_from_fov(fov(-half, half, half, -half), 0.05, 100.0),
        };
        let m = vp.view_projection();
        let p = [0.0f32, 0.0, 1.0, 1.0];
        let w: f32 = (0..4).map(|k| m.columns[k][3] * p[k]).sum();
        assert!(w < 0.0, "a point behind the eye must not be drawn");
    }

    #[test]
    fn the_model_matrix_gives_the_surface_its_metre_extent() {
        // 幅 1.6m 高さ 0.9m の面のローカル ±0.5 が、世界で ±0.8 / ±0.45 になること。
        let size = SurfaceSize::new(1.6, 0.9).expect("positive");
        let m = Mat4::from_pose_scaled(Pose::IDENTITY, size.width_m(), size.height_m());
        let corner = [0.5f32, 0.5, 0.0, 1.0];
        let mut out = [0.0f32; 4];
        for (r, cell) in out.iter_mut().enumerate() {
            *cell = (0..4).map(|k| m.columns[k][r] * corner[k]).sum();
        }
        assert!((out[0] - 0.8).abs() < 1e-6);
        assert!((out[1] - 0.45).abs() < 1e-6);
    }

    #[test]
    fn the_view_matrix_undoes_the_eye_pose() {
        // 目が (0,0,2) に在るとき、世界の (0,0,1) は目の座標で (0,0,-1) になる。
        let pose = Pose::new(Vec3::new(0.0, 0.0, 2.0), Quat::IDENTITY);
        let v = Mat4::view_from_pose(pose);
        let p = [0.0f32, 0.0, 1.0, 1.0];
        let mut out = [0.0f32; 4];
        for (r, cell) in out.iter_mut().enumerate() {
            *cell = (0..4).map(|k| v.columns[k][r] * p[k]).sum();
        }
        assert!((out[2] - -1.0).abs() < 1e-6, "got {out:?}");
    }

    #[test]
    fn a_surface_cannot_have_a_zero_or_negative_extent() {
        assert!(SurfaceSize::new(0.0, 1.0).is_err());
        assert!(SurfaceSize::new(1.0, -1.0).is_err());
        assert!(SurfaceSize::new(f32::NAN, 1.0).is_err());
    }

    #[test]
    fn the_near_plane_must_be_positive() {
        let view = openxr::View {
            pose: openxr::Posef::IDENTITY,
            fov: fov(-0.5, 0.5, 0.5, -0.5),
        };
        assert!(ViewProjection::from_openxr(view, 0.0, 100.0).is_err());
        assert!(ViewProjection::from_openxr(view, 0.05, 100.0).is_ok());
    }

    #[test]
    fn matrix_multiplication_has_identity_on_both_sides() {
        let half = core::f32::consts::FRAC_PI_4;
        let p = projection_from_fov(fov(-half, half, half, -half), 0.05, 100.0);
        assert_eq!(p.mul(Mat4::IDENTITY), p);
        assert_eq!(Mat4::IDENTITY.mul(p), p);
    }
}
