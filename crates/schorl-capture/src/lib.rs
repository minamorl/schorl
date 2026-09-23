//! `schorl-capture` — Linux 側の出力から実フレームを取る。
//!
//! **spec 0.2 の原文3「360度あるならそれ用にウィンドウマネージャー作るだけでいいのでは？」
//! でこの経路は v1 から外れた。消していないのは、これがこの repo で唯一の実測資産
//! (実ホストで wlr-screencopy v3 から実フレームが取れた証拠) であり、
//! 既存 Hyprland の窓を VR から見る将来の口でもあるため。**
//!
//! 満たしていた pin (いずれも原文3 で退役):
//! - `v1.panel_source: require schorl.v1.panel.content_source = linux_display` —
//!   [`FrameSource`] は [`OutputId`] を指定して取る。取り口はそれだけ。
//! - `verify.machine_scope` の `capture_returns_real_frame` — [`FrameOrigin`] が
//!   本物か贋物かを持ち歩くので、贋物のフレームで「捕捉できた」と言えない。
//!   (0.2 でこの項目は `client_frame_reaches_swapchain` へ改鍵された。)
//! - `house.effect_boundary.*` — 捕捉だけを持つ最小の capability。
//!
//! 絵そのものを表す [`Frame`] / [`FrameOrigin`] / [`PixelFormat`] は
//! [`schorl_core::frame`] へ移した。`client_frame_reaches_swapchain` は取り口に
//! 依存しないので、v1 の経路がこの crate を通らずに絵を運べる必要がある。
//! ここでは同じ名前で再輸出しているだけで、公開面も振る舞いも変えていない。
//! 実ホスト向けの実装は [`screencopy`] にある (wlr-screencopy v3)。

pub mod screencopy;

use schorl_core::error::Result;
use schorl_core::time::UtcTimestamp;
use schorl_display::OutputId;

pub use schorl_core::frame::{Frame, FrameOrigin, PixelFormat};

/// 出力から一枚取る capability。
///
/// 実ホスト向けの実装は [`screencopy::ScreencopyFrameSource`]、
/// 差し替え用の贋物は [`testing::SolidColourFrameSource`]。
pub trait FrameSource: Send {
    /// 指定した出力から一枚取る。
    fn capture(&mut self, output: &OutputId) -> Result<Frame>;
}

/// 試験用の差し替え実装 (`house.effect_boundary.substitution`)。
pub mod testing {
    use super::*;

    /// 単色のフレームを返す贋物。出所は必ず [`FrameOrigin::TestDouble`]。
    #[derive(Debug, Clone)]
    pub struct SolidColourFrameSource {
        /// 横の画素数。
        pub width_px: u32,
        /// 縦の画素数。
        pub height_px: u32,
        /// 埋める byte。
        pub fill: u8,
        /// 取れたことにする時刻。
        pub captured_at: UtcTimestamp,
    }

    impl FrameSource for SolidColourFrameSource {
        fn capture(&mut self, _output: &OutputId) -> Result<Frame> {
            let format = PixelFormat::Xrgb8888;
            let stride = self.width_px.saturating_mul(format.bytes_per_pixel());
            let pixels = vec![self.fill; (stride as usize).saturating_mul(self.height_px as usize)];
            Frame::new(
                FrameOrigin::TestDouble,
                format,
                self.width_px,
                self.height_px,
                stride,
                self.captured_at,
                pixels,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::SolidColourFrameSource;
    use super::*;

    fn at(millis: i64) -> UtcTimestamp {
        UtcTimestamp::from_millis_since_epoch(millis)
    }

    #[test]
    fn a_test_double_frame_is_not_a_real_capture() {
        let mut source = SolidColourFrameSource {
            width_px: 4,
            height_px: 2,
            fill: 0x20,
            captured_at: at(1),
        };
        let output = OutputId::new("SCHORL-1").expect("valid name");
        let frame = source.capture(&output).expect("captured");
        assert_eq!(frame.origin(), FrameOrigin::TestDouble);
        assert!(!frame.is_real_capture());
        assert_eq!(frame.pixels().len(), 4 * 4 * 2);
    }
}
