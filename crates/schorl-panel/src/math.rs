//! 板を置き直すために要る最小の剛体変換。
//!
//! 外部の数学 crate を借りていないのは、この phase で依存の一次資料を読む経路を
//! 持っていないため (`P-EX-2`)。ここに在るのは四元数の積・共役・回転という
//! 定義どおりの演算だけで、外部仕様ではない。
//!
//! 座標系は右手系。`+X` が右、`+Y` が上、`-Z` が前 (板の表が向く先)。

/// 3 次元ベクトル。単位はメートル。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    /// 右方向成分。
    pub x: f32,
    /// 上方向成分。
    pub y: f32,
    /// 手前方向成分。
    pub z: f32,
}

impl Vec3 {
    /// 原点。
    pub const ZERO: Vec3 = Vec3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// 成分から作る。
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    /// 和。
    pub const fn add(self, other: Vec3) -> Vec3 {
        Vec3::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }

    /// 差。
    pub const fn sub(self, other: Vec3) -> Vec3 {
        Vec3::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }

    /// スカラー倍。
    pub const fn scale(self, k: f32) -> Vec3 {
        Vec3::new(self.x * k, self.y * k, self.z * k)
    }

    /// 内積。
    pub const fn dot(self, other: Vec3) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// 外積。
    pub const fn cross(self, other: Vec3) -> Vec3 {
        Vec3::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }

    /// 長さ。
    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }
}

/// 単位四元数で表した向き。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quat {
    /// i 成分。
    pub x: f32,
    /// j 成分。
    pub y: f32,
    /// k 成分。
    pub z: f32,
    /// 実部。
    pub w: f32,
}

impl Quat {
    /// 回転なし。
    pub const IDENTITY: Quat = Quat {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        w: 1.0,
    };

    /// 成分から作る。正規化はしない。
    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    /// 単位軸まわりの回転。`axis` は単位ベクトルであること。
    pub fn from_axis_angle(axis: Vec3, radians: f32) -> Self {
        let half = radians * 0.5;
        let s = half.sin();
        Quat::new(axis.x * s, axis.y * s, axis.z * s, half.cos())
    }

    /// 合成。`self` のあとに適用されるのではなく、`self * other` の順。
    pub const fn mul(self, other: Quat) -> Quat {
        Quat::new(
            self.w * other.x + self.x * other.w + self.y * other.z - self.z * other.y,
            self.w * other.y - self.x * other.z + self.y * other.w + self.z * other.x,
            self.w * other.z + self.x * other.y - self.y * other.x + self.z * other.w,
            self.w * other.w - self.x * other.x - self.y * other.y - self.z * other.z,
        )
    }

    /// 共役。単位四元数では逆回転に等しい。
    pub const fn conjugate(self) -> Quat {
        Quat::new(-self.x, -self.y, -self.z, self.w)
    }

    /// ベクトルを回す。
    pub const fn rotate(self, v: Vec3) -> Vec3 {
        let q = Vec3::new(self.x, self.y, self.z);
        let t = q.cross(v).scale(2.0);
        v.add(t.scale(self.w)).add(q.cross(t))
    }
}

impl Default for Quat {
    fn default() -> Self {
        Quat::IDENTITY
    }
}

/// 位置と向きの組。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pose {
    /// 位置。
    pub position: Vec3,
    /// 向き。
    pub orientation: Quat,
}

impl Pose {
    /// 原点・無回転。
    pub const IDENTITY: Pose = Pose {
        position: Vec3::ZERO,
        orientation: Quat::IDENTITY,
    };

    /// 組から作る。
    pub const fn new(position: Vec3, orientation: Quat) -> Self {
        Self {
            position,
            orientation,
        }
    }

    /// `self` の座標系で表された `local` を親の座標系へ持ち上げる。
    pub const fn compose(self, local: Pose) -> Pose {
        Pose::new(
            self.position.add(self.orientation.rotate(local.position)),
            self.orientation.mul(local.orientation),
        )
    }

    /// 逆変換。
    pub const fn inverse(self) -> Pose {
        let inv = self.orientation.conjugate();
        Pose::new(inv.rotate(self.position).scale(-1.0), inv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn close_vec(a: Vec3, b: Vec3) -> bool {
        close(a.x, b.x) && close(a.y, b.y) && close(a.z, b.z)
    }

    #[test]
    fn quarter_turn_about_y_sends_right_to_back() {
        let q = Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), std::f32::consts::FRAC_PI_2);
        let rotated = q.rotate(Vec3::new(1.0, 0.0, 0.0));
        assert!(
            close_vec(rotated, Vec3::new(0.0, 0.0, -1.0)),
            "got {rotated:?}"
        );
    }

    #[test]
    fn identity_rotation_keeps_the_vector() {
        let v = Vec3::new(1.0, -2.0, 3.0);
        assert!(close_vec(Quat::IDENTITY.rotate(v), v));
    }

    #[test]
    fn pose_composed_with_its_inverse_is_the_identity() {
        let pose = Pose::new(
            Vec3::new(0.3, 1.2, -0.8),
            Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), 0.7),
        );
        let round_trip = pose.compose(pose.inverse());
        assert!(close_vec(round_trip.position, Vec3::ZERO));
        assert!(close(round_trip.orientation.w.abs(), 1.0));
    }

    #[test]
    fn dot_and_cross_follow_the_right_hand_rule() {
        let x = Vec3::new(1.0, 0.0, 0.0);
        let y = Vec3::new(0.0, 1.0, 0.0);
        assert!(close(x.dot(y), 0.0));
        assert!(close_vec(x.cross(y), Vec3::new(0.0, 0.0, 1.0)));
        assert!(close(Vec3::new(3.0, 4.0, 0.0).length(), 5.0));
    }
}
