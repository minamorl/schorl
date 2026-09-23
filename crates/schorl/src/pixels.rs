//! compositor 面が受け取ったクライアントの画素を、描画面へ渡す継ぎ目。
//!
//! `schorl-compositor` は [`ClientTextureImporter`] という口を開けて
//! 「受け取った」までを持ち、`schorl-render` は `VkImage` を作る手を持つ。
//! **どちらの crate も相手を知らない。** 両方を知っているのはこの境界だけで、
//! それが `house.effect_boundary.location = injected_at_boundary` の形である。
//!
//! ここが埋めるのは `pin verify.machine_scope` の
//! `client_frame_reaches_swapchain` の前半 — クライアントの画素が描画面の
//! 手に渡るところまで。後半 (テクスチャにして swapchain へ描く) は
//! [`crate::stage`] が持つ。
//!
//! # 画素を写して持つ理由
//!
//! [`ShmBufferView`] はクライアントの pool を借りているだけなので、commit の
//! 閉包より長く生きられない (`house.resource_lifecycle.explicit_escape`)。
//! テクスチャを作るのは描画の番なので、ここで一度だけ写す。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use schorl_compositor::window::WindowId;
use schorl_compositor::{ClientTextureImporter, Dmabuf, DmabufDimensions, ShmBufferView};
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::frame::{Frame, FrameOrigin, PixelFormat};
use schorl_core::id::TraceId;
use schorl_core::time::Clock;
use schorl_render::dmabuf::{DmabufDescriptor, DmabufPlane, DrmFormat, DrmModifier};

/// `wl_shm` の `ARGB8888`。値は `wayland.xml` の逐語 (`format` enum の 0)。
pub const WL_SHM_ARGB8888: u32 = 0;
/// `wl_shm` の `XRGB8888`。値は `wayland.xml` の逐語 (`format` enum の 1)。
pub const WL_SHM_XRGB8888: u32 = 1;

/// クライアントが最後に出した一枚。
///
/// 二枝あるのは `free schorl.compositor.buffer_import_path` を片道にしないため。
/// dmabuf が使えない構成でも shm で絵が出る。
#[derive(Debug)]
pub enum ClientBuffer {
    /// CPU 側へ写し取った画素。
    Shm(Frame),
    /// fd ごと引き受けた dmabuf の記述。
    Dmabuf(DmabufDescriptor),
}

impl ClientBuffer {
    /// ログに出す綴り。
    pub const fn kind(&self) -> &'static str {
        match self {
            ClientBuffer::Shm(_) => "shm",
            ClientBuffer::Dmabuf(_) => "dmabuf",
        }
    }
}

/// 一枚と、その通し番号。
///
/// 番号は「同じ絵を何度もテクスチャにしない」ためだけに要る。
#[derive(Debug)]
pub struct ClientFrame {
    /// 受け取った順の通し番号。
    pub generation: u64,
    /// 中身。
    pub buffer: ClientBuffer,
}

#[derive(Debug, Default)]
struct Inbox {
    latest: HashMap<WindowId, ClientFrame>,
    next_generation: u64,
    refused: Vec<String>,
}

/// compositor 側に差し込む引き渡し先。
///
/// `ClientTextureImporter` の実装はこれ一つで、GPU には触らない。触るのは
/// [`PixelInbox`] から取り出した側である。
pub struct ClientPixelRelay {
    inbox: Arc<Mutex<Inbox>>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for ClientPixelRelay {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ClientPixelRelay { .. }")
    }
}

/// 取り出す側。描画の輪が毎回ここを見る。
#[derive(Debug, Clone)]
pub struct PixelInbox {
    inbox: Arc<Mutex<Inbox>>,
}

/// 引き渡し先と取り出し口を一対で作る。
pub fn relay(clock: Arc<dyn Clock>) -> (ClientPixelRelay, PixelInbox) {
    let inbox = Arc::new(Mutex::new(Inbox::default()));
    (
        ClientPixelRelay {
            inbox: Arc::clone(&inbox),
            clock,
        },
        PixelInbox { inbox },
    )
}

impl PixelInbox {
    /// そのウィンドウの絵が `seen` より新しければ取り出す。
    ///
    /// 取り出したら受け箱からは消える。dmabuf は fd を所有しているので、
    /// 写しを二つ持たない (`house.resource_lifecycle.explicit_escape`)。
    pub fn take_newer_than(&self, window: &WindowId, seen: u64) -> Option<ClientFrame> {
        let mut inbox = self.inbox.lock().ok()?;
        let fresh = inbox
            .latest
            .get(window)
            .is_some_and(|frame| frame.generation > seen);
        if fresh {
            inbox.latest.remove(window)
        } else {
            None
        }
    }

    /// 受け取れなかった物の記録。空でないことは失敗の証拠になる。
    pub fn refusals(&self) -> Vec<String> {
        self.inbox
            .lock()
            .map(|inbox| inbox.refused.clone())
            .unwrap_or_default()
    }

    /// これまでに受け取った枚数。
    pub fn accepted(&self) -> u64 {
        self.inbox
            .lock()
            .map(|inbox| inbox.next_generation)
            .unwrap_or(0)
    }
}

impl ClientPixelRelay {
    fn store(&mut self, window: &WindowId, buffer: ClientBuffer) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.next_generation += 1;
            let generation = inbox.next_generation;
            inbox
                .latest
                .insert(window.clone(), ClientFrame { generation, buffer });
        }
    }

    fn refuse(&mut self, reason: String) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.refused.push(reason);
        }
    }
}

/// `wl_shm` の format 値を [`PixelFormat`] へ。
///
/// 二つしか受けない。`wl_shm` は必須の二つ以外も名乗れるが、写す先が無い値を
/// 黙って別の並びとして読むのが一番まずい。
pub const fn pixel_format_of_wl_shm(format: u32) -> Option<PixelFormat> {
    match format {
        WL_SHM_ARGB8888 => Some(PixelFormat::Argb8888),
        WL_SHM_XRGB8888 => Some(PixelFormat::Xrgb8888),
        _ => None,
    }
}

impl ClientTextureImporter for ClientPixelRelay {
    fn import_shm(&mut self, window: &WindowId, view: ShmBufferView<'_>) -> Result<()> {
        let Some(format) = pixel_format_of_wl_shm(view.wl_shm_format) else {
            let reason = format!(
                "window {} committed a wl_shm buffer in format {}, which has no pixel order here",
                window.as_str(),
                view.wl_shm_format
            );
            self.refuse(reason.clone());
            return Err(Error::new(
                ErrorCode::Unsupported,
                "this wl_shm format has no pixel order in the v1 mapping",
                TraceId::unattributed(),
            )
            .with_detail("wl_shm_format", i64::from(view.wl_shm_format)));
        };
        let width_px = u32::try_from(view.width_px).map_err(|_| bad_extent(&view))?;
        let height_px = u32::try_from(view.height_px).map_err(|_| bad_extent(&view))?;
        let stride_bytes = u32::try_from(view.stride_bytes).map_err(|_| bad_extent(&view))?;
        let offset = usize::try_from(view.offset_bytes).map_err(|_| bad_extent(&view))?;
        let wanted = (stride_bytes as usize).saturating_mul(height_px as usize);
        let end = offset.saturating_add(wanted);
        if end > view.bytes.len() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "the client's shm pool is shorter than the buffer it described",
                TraceId::unattributed(),
            )
            .with_detail("needed", end as i64)
            .with_detail("pool_bytes", view.bytes.len() as i64));
        }
        // ここで一度だけ写す。借用は commit の閉包より長く生きられない。
        let pixels = view.bytes[offset..end].to_vec();
        let frame = Frame::new(
            // ホストの別 compositor から取った絵ではなく、schorl 自身の
            // ソケットへ実在のクライアントが出した絵である。**贋物ではない**
            // ことをこの枝が持つ (`FrameOrigin::TestDouble` と区別する唯一の口)。
            FrameOrigin::RealCapture,
            format,
            width_px,
            height_px,
            stride_bytes,
            self.clock.now_utc(),
            pixels,
        )?;
        self.store(window, ClientBuffer::Shm(frame));
        Ok(())
    }

    fn import_dmabuf(&mut self, window: Option<&WindowId>, dmabuf: &Dmabuf) -> Result<()> {
        // 宛先の分からない段 (`zwp_linux_buffer_params_v1.create`) は受けるだけ。
        // 貼る先が無いので写しも取らない。
        let Some(window) = window else {
            return Ok(());
        };
        let descriptor = describe(dmabuf)?;
        self.store(window, ClientBuffer::Dmabuf(descriptor));
        Ok(())
    }
}

fn bad_extent(view: &ShmBufferView<'_>) -> Error {
    Error::new(
        ErrorCode::InvalidArgument,
        "the client described a shm buffer with a negative extent",
        TraceId::unattributed(),
    )
    .with_detail("width_px", i64::from(view.width_px))
    .with_detail("height_px", i64::from(view.height_px))
    .with_detail("stride_bytes", i64::from(view.stride_bytes))
}

/// smithay が組み立てた dmabuf を、import できる記述へ写す。
///
/// fd は複製して所有する。元の [`Dmabuf`] はクライアントの物なので、
/// こちらが閉じてはならない。
pub fn describe(dmabuf: &Dmabuf) -> Result<DmabufDescriptor> {
    let format = dmabuf.format();
    let size = dmabuf.size();
    let width_px = u32::try_from(size.w).map_err(|_| negative_dmabuf(size.w, size.h))?;
    let height_px = u32::try_from(size.h).map_err(|_| negative_dmabuf(size.w, size.h))?;
    let mut planes = Vec::with_capacity(dmabuf.num_planes());
    for ((fd, offset), stride) in dmabuf
        .handles()
        .zip(dmabuf.offsets())
        .zip(dmabuf.strides())
    {
        let owned = fd.try_clone_to_owned().map_err(|e| {
            Error::new(
                ErrorCode::HostRefused,
                "the client's dma_buf file descriptor could not be duplicated",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })?;
        planes.push(DmabufPlane::new(
            owned,
            u64::from(offset),
            u64::from(stride),
        ));
    }
    Ok(DmabufDescriptor {
        width_px,
        height_px,
        format: DrmFormat::from_u32(format.code as u32),
        modifier: DrmModifier::from_u64(u64::from(format.modifier)),
        planes,
    })
}

fn negative_dmabuf(w: i32, h: i32) -> Error {
    Error::new(
        ErrorCode::InvalidArgument,
        "the client described a dma_buf with a negative extent",
        TraceId::unattributed(),
    )
    .with_detail("width_px", i64::from(w))
    .with_detail("height_px", i64::from(h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_core::id::{Id, IdScheme};
    use schorl_core::time::SystemClock;

    fn window(text: &str) -> WindowId {
        WindowId::new(Id::new(IdScheme::Uuidv7, text).expect("non empty"))
    }

    fn view<'a>(bytes: &'a [u8], format: u32) -> ShmBufferView<'a> {
        ShmBufferView {
            wl_shm_format: format,
            width_px: 2,
            height_px: 2,
            stride_bytes: 8,
            offset_bytes: 0,
            bytes,
        }
    }

    #[test]
    fn the_two_mandatory_wl_shm_formats_map_and_the_rest_do_not() {
        assert_eq!(
            pixel_format_of_wl_shm(WL_SHM_ARGB8888),
            Some(PixelFormat::Argb8888)
        );
        assert_eq!(
            pixel_format_of_wl_shm(WL_SHM_XRGB8888),
            Some(PixelFormat::Xrgb8888)
        );
        assert_eq!(pixel_format_of_wl_shm(0x34324742), None);
    }

    #[test]
    fn a_committed_shm_buffer_reaches_the_inbox_with_the_client_pixels() {
        let (mut relay_side, inbox) = relay(Arc::new(SystemClock));
        let bytes: Vec<u8> = (0..16u8).collect();
        let id = window("w-1");
        relay_side
            .import_shm(&id, view(&bytes, WL_SHM_XRGB8888))
            .expect("accepted");

        let got = inbox.take_newer_than(&id, 0).expect("a frame arrived");
        assert_eq!(got.generation, 1);
        match got.buffer {
            ClientBuffer::Shm(frame) => {
                assert_eq!(frame.origin(), FrameOrigin::RealCapture);
                assert_eq!(frame.pixels(), &bytes[..]);
            }
            other => panic!("expected shm, got {}", other.kind()),
        }
        assert!(inbox.take_newer_than(&id, 0).is_none(), "taken once");
    }

    #[test]
    fn an_older_generation_is_not_handed_out_twice() {
        let (mut relay_side, inbox) = relay(Arc::new(SystemClock));
        let bytes = vec![0u8; 16];
        let id = window("w-1");
        relay_side
            .import_shm(&id, view(&bytes, WL_SHM_XRGB8888))
            .expect("accepted");
        relay_side
            .import_shm(&id, view(&bytes, WL_SHM_XRGB8888))
            .expect("accepted");
        assert!(
            inbox.take_newer_than(&id, 2).is_none(),
            "generation 2 is not newer than 2"
        );
        assert_eq!(
            inbox.take_newer_than(&id, 1).expect("newer").generation,
            2
        );
    }

    #[test]
    fn an_unmappable_shm_format_is_refused_in_an_envelope_and_recorded() {
        let (mut relay_side, inbox) = relay(Arc::new(SystemClock));
        let bytes = vec![0u8; 16];
        let id = window("w-1");
        let err = relay_side
            .import_shm(&id, view(&bytes, 0x34324742))
            .expect_err("no pixel order");
        assert_eq!(err.code(), ErrorCode::Unsupported);
        assert_eq!(inbox.refusals().len(), 1);
        assert!(inbox.take_newer_than(&id, 0).is_none());
    }

    #[test]
    fn a_pool_shorter_than_the_described_buffer_is_refused() {
        let (mut relay_side, _inbox) = relay(Arc::new(SystemClock));
        let bytes = vec![0u8; 8];
        let id = window("w-1");
        let err = relay_side
            .import_shm(&id, view(&bytes, WL_SHM_XRGB8888))
            .expect_err("the pool is too short");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn two_windows_keep_their_own_latest_frame() {
        let (mut relay_side, inbox) = relay(Arc::new(SystemClock));
        let bytes = vec![7u8; 16];
        let a = window("a");
        let b = window("b");
        relay_side
            .import_shm(&a, view(&bytes, WL_SHM_XRGB8888))
            .expect("accepted");
        relay_side
            .import_shm(&b, view(&bytes, WL_SHM_XRGB8888))
            .expect("accepted");
        assert!(inbox.take_newer_than(&a, 0).is_some());
        assert!(inbox.take_newer_than(&b, 0).is_some());
        assert_eq!(inbox.accepted(), 2);
    }
}
