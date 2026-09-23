//! 画素の一枚。取り口ではなく、絵そのもの。
//!
//! spec 0.2 (原文3) までこの三つの型は `schorl-capture` に住んでいた。原文3 で
//! 「他 compositor の画面を capture して板に貼る」経路が v1 から外れたが、
//! *絵そのもの* は外れていない: pin `verify.machine_scope` の
//! `client_frame_reaches_swapchain` が、クライアントの一枚が swapchain へ届くことを
//! 機械検査に数えている。取り口 ([`FrameSource`](../../schorl_capture/trait.FrameSource.html))
//! は capture 側に残し、絵だけをここへ移した。移動であって書き換えではない。
//!
//! `schorl-capture` は同じ名前で再輸出しているので、あちらの公開面は変わっていない。

use crate::error::{Error, ErrorCode, Result};
use crate::id::TraceId;
use crate::time::UtcTimestamp;

/// 画素の並び。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8bit ずつの BGRX (little endian の 0xXXRRGGBB)。
    Xrgb8888,
    /// 8bit ずつの BGRA。
    Argb8888,
}

impl PixelFormat {
    /// 1 画素の byte 数。
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelFormat::Xrgb8888 | PixelFormat::Argb8888 => 4,
        }
    }
}

/// そのフレームがどこから来たか。
///
/// 「実フレームが届いた」という機械検査を、贋物で緑にさせないために要る。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameOrigin {
    /// ホストの compositor から実際に取れた絵。
    RealCapture,
    /// 試験用に組み立てた絵。実フレームとは数えない。
    TestDouble,
}

/// 取れた一枚。
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    origin: FrameOrigin,
    format: PixelFormat,
    width_px: u32,
    height_px: u32,
    stride_bytes: u32,
    captured_at: UtcTimestamp,
    pixels: Vec<u8>,
}

impl Frame {
    /// 画素の入れ物と寸法から作る。寸法が合わなければ封筒で拒む。
    pub fn new(
        origin: FrameOrigin,
        format: PixelFormat,
        width_px: u32,
        height_px: u32,
        stride_bytes: u32,
        captured_at: UtcTimestamp,
        pixels: Vec<u8>,
    ) -> Result<Self> {
        if width_px == 0 || height_px == 0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "frame must have a non-zero extent",
                TraceId::unattributed(),
            ));
        }
        let minimum_stride = width_px.saturating_mul(format.bytes_per_pixel());
        if stride_bytes < minimum_stride {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "frame stride is smaller than one row of pixels",
                TraceId::unattributed(),
            )
            .with_detail("stride_bytes", i64::from(stride_bytes))
            .with_detail("minimum_stride", i64::from(minimum_stride)));
        }
        let expected = (stride_bytes as usize).saturating_mul(height_px as usize);
        if pixels.len() != expected {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "frame buffer length does not match stride times height",
                TraceId::unattributed(),
            )
            .with_detail("expected", expected as i64)
            .with_detail("actual", pixels.len() as i64));
        }
        Ok(Self {
            origin,
            format,
            width_px,
            height_px,
            stride_bytes,
            captured_at,
            pixels,
        })
    }

    /// 出所。
    pub const fn origin(&self) -> FrameOrigin {
        self.origin
    }

    /// ホストから実際に取れた絵か。
    pub const fn is_real_capture(&self) -> bool {
        matches!(self.origin, FrameOrigin::RealCapture)
    }

    /// 画素の並び。
    pub const fn format(&self) -> PixelFormat {
        self.format
    }

    /// 横の画素数。
    pub const fn width_px(&self) -> u32 {
        self.width_px
    }

    /// 縦の画素数。
    pub const fn height_px(&self) -> u32 {
        self.height_px
    }

    /// 行あたりの byte 数。
    pub const fn stride_bytes(&self) -> u32 {
        self.stride_bytes
    }

    /// 取れた時刻 (UTC)。
    pub const fn captured_at(&self) -> UtcTimestamp {
        self.captured_at
    }

    /// 画素。
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: i64) -> UtcTimestamp {
        UtcTimestamp::from_millis_since_epoch(millis)
    }

    #[test]
    fn a_real_frame_reports_itself_as_real() {
        let frame = Frame::new(
            FrameOrigin::RealCapture,
            PixelFormat::Argb8888,
            2,
            2,
            8,
            at(5),
            vec![0; 16],
        )
        .expect("valid frame");
        assert!(frame.is_real_capture());
        assert_eq!(frame.captured_at(), at(5));
        assert_eq!(frame.stride_bytes(), 8);
    }

    #[test]
    fn a_test_double_frame_is_not_a_real_capture() {
        let frame = Frame::new(
            FrameOrigin::TestDouble,
            PixelFormat::Xrgb8888,
            2,
            2,
            8,
            at(5),
            vec![0; 16],
        )
        .expect("valid frame");
        assert_eq!(frame.origin(), FrameOrigin::TestDouble);
        assert!(!frame.is_real_capture());
    }

    #[test]
    fn a_buffer_that_does_not_match_the_stride_is_refused() {
        let err = Frame::new(
            FrameOrigin::RealCapture,
            PixelFormat::Xrgb8888,
            2,
            2,
            8,
            at(0),
            vec![0; 15],
        )
        .expect_err("short buffer");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn a_stride_narrower_than_one_row_is_refused() {
        let err = Frame::new(
            FrameOrigin::RealCapture,
            PixelFormat::Xrgb8888,
            4,
            1,
            8,
            at(0),
            vec![0; 8],
        )
        .expect_err("narrow stride");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn a_zero_sized_frame_is_refused() {
        let err = Frame::new(
            FrameOrigin::RealCapture,
            PixelFormat::Xrgb8888,
            0,
            1,
            0,
            at(0),
            Vec::new(),
        )
        .expect_err("zero extent");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }
}
