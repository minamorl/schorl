//! クライアントが提出したバッファを受け取り、次の面へ引き渡す口。
//!
//! shm と dmabuf の両方を受ける。**ここは import しない。**
//! dmabuf を GPU テクスチャへ実際に取り込むのは別の面の仕事であり、
//! この crate はその面へ渡す capability を境界で受け取るだけである
//! (`house.effect_boundary.form = minimal_explicit_capability_or_adapter`,
//!  `house.effect_boundary.location = injected_at_boundary`)。
//!
//! 口を二本 (`import_shm` / `import_dmabuf`) に絞ってあるのは
//! `house.effect_boundary.no_god_capability` のため。差し替えられることは
//! [`RecordingImporter`] と実装側の二つで担保する
//! (`house.effect_boundary.substitution`)。

use std::sync::{Arc, Mutex};

use schorl_core::error::Result;
use crate::window::WindowId;

/// smithay が組み立てたクライアントの dmabuf。
///
/// [`ClientTextureImporter`] を実装する面はこの crate の外に居る (GPU へ取り込む
/// のは別の面の仕事である)。その面が smithay を直に依存せずに署名を書けるよう、
/// 受け取る型をここから出しておく。
pub use smithay::backend::allocator::dmabuf::Dmabuf;
/// dmabuf の寸法と format を読むための trait。
pub use smithay::backend::allocator::Buffer as DmabufDimensions;

/// クライアントが出してきたバッファの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientBufferKind {
    /// `wl_shm` の共有メモリ。画素は CPU から読める。
    Shm,
    /// `zwp_linux_dmabuf_v1` の dmabuf。画素は fd の向こうにある。
    Dmabuf,
}

impl ClientBufferKind {
    /// ログと封筒に出す綴り。
    pub const fn as_str(self) -> &'static str {
        match self {
            ClientBufferKind::Shm => "shm",
            ClientBufferKind::Dmabuf => "dmabuf",
        }
    }
}

/// shm バッファの一枚を、所有せずに覗くための窓。
///
/// `bytes` はクライアントの pool を指しているので、この借用より長く持ち出せない。
/// 持ち出したい面は自分で複写する (`house.resource_lifecycle.explicit_escape`)。
#[derive(Debug, Clone, Copy)]
pub struct ShmBufferView<'a> {
    /// `wl_shm` の format 値 (DRM FourCC ではない)。
    pub wl_shm_format: u32,
    /// 幅 (画素)。
    pub width_px: i32,
    /// 高さ (画素)。
    pub height_px: i32,
    /// 一行の byte 数。
    pub stride_bytes: i32,
    /// pool 内の先頭からの byte offset。
    pub offset_bytes: i32,
    /// pool 全体の借用。`offset_bytes` から読むこと。
    pub bytes: &'a [u8],
}

/// dmabuf 一枚の素性。
///
/// fd そのものは [`Dmabuf`] が握ったままにする。ここは「何が来たか」を
/// ログと試験が見るための写しであって、画素の所有権ではない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmabufHandoff {
    /// DRM FourCC を 32bit の生値で。
    pub fourcc: u32,
    /// DRM format modifier の生値。
    pub modifier: u64,
    /// 幅 (画素)。
    pub width_px: i32,
    /// 高さ (画素)。
    pub height_px: i32,
    /// plane の本数。
    pub plane_count: usize,
}

impl DmabufHandoff {
    /// smithay が組み立てた [`Dmabuf`] から素性だけを写し取る。
    pub fn describe(dmabuf: &Dmabuf) -> Self {
        let format = dmabuf.format();
        let size = dmabuf.size();
        Self {
            fourcc: format.code as u32,
            modifier: u64::from(format.modifier),
            width_px: size.w,
            height_px: size.h,
            plane_count: dmabuf.num_planes(),
        }
    }
}

/// クライアントの画素を受け取って次の面へ渡す capability。
///
/// この crate はこの trait を **呼ぶ側** であり、実装は持たない
/// (試験用の [`RecordingImporter`] を除く)。
pub trait ClientTextureImporter: Send {
    /// shm の一枚を受け取る。
    fn import_shm(&mut self, window: &WindowId, view: ShmBufferView<'_>) -> Result<()>;

    /// dmabuf の一枚を受け取る。
    ///
    /// `window` が `None` なのは、dmabuf がまだどの surface にも attach されて
    /// いない段 (`zwp_linux_buffer_params_v1.create` の時点) だからである。
    /// 宛先が分かってから来る shm 側と違い、ここは素性だけが分かる。
    ///
    /// `Ok` を返したら、クライアントへは `zwp_linux_buffer_params_v1.created` で
    /// 成功が返る。`Err` ならその封筒をログへ出したうえで失敗を返す。
    fn import_dmabuf(&mut self, window: Option<&WindowId>, dmabuf: &Dmabuf) -> Result<()>;
}

/// 何を受け取ったかの一覧。
type SeenBuffers = Arc<Mutex<Vec<(Option<WindowId>, ClientBufferKind)>>>;

/// 受け取った物を数えるだけの引き渡し先。
///
/// GPU 側がまだ繋がっていない段の既定であり、試験の目でもある。
/// **画素はどこへも取り込まれない。** 「受け取れた」ことだけを記録する。
#[derive(Debug, Clone, Default)]
pub struct RecordingImporter {
    seen: SeenBuffers,
    dmabufs: Arc<Mutex<Vec<DmabufHandoff>>>,
}

impl RecordingImporter {
    /// 空の記録係を作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// これまでに受け取った (ウィンドウ, 種別) の並び。
    pub fn seen(&self) -> Vec<(Option<WindowId>, ClientBufferKind)> {
        self.seen.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// これまでに受け取った dmabuf の素性。
    pub fn dmabufs(&self) -> Vec<DmabufHandoff> {
        self.dmabufs.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// 受け取った枚数。
    pub fn count(&self) -> usize {
        self.seen.lock().map(|v| v.len()).unwrap_or(0)
    }

    fn record(&self, window: Option<&WindowId>, kind: ClientBufferKind) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push((window.cloned(), kind));
        }
    }
}

impl ClientTextureImporter for RecordingImporter {
    fn import_shm(&mut self, window: &WindowId, _view: ShmBufferView<'_>) -> Result<()> {
        self.record(Some(window), ClientBufferKind::Shm);
        Ok(())
    }

    fn import_dmabuf(&mut self, window: Option<&WindowId>, dmabuf: &Dmabuf) -> Result<()> {
        self.record(window, ClientBufferKind::Dmabuf);
        if let Ok(mut described) = self.dmabufs.lock() {
            described.push(DmabufHandoff::describe(dmabuf));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_core::id::{Id, IdScheme};

    fn window(text: &str) -> WindowId {
        WindowId::new(Id::new(IdScheme::Uuidv7, text).expect("non empty"))
    }

    #[test]
    fn the_recording_importer_keeps_both_kinds_apart() {
        let mut importer = RecordingImporter::new();
        let w = window("w-1");
        importer
            .import_shm(
                &w,
                ShmBufferView {
                    wl_shm_format: 0,
                    width_px: 4,
                    height_px: 2,
                    stride_bytes: 16,
                    offset_bytes: 0,
                    bytes: &[0u8; 32],
                },
            )
            .expect("accepted");

        assert_eq!(importer.count(), 1);
        assert_eq!(importer.seen()[0].1, ClientBufferKind::Shm);
        assert!(importer.dmabufs().is_empty());
    }

    #[test]
    fn buffer_kind_spelling_is_stable() {
        assert_eq!(ClientBufferKind::Shm.as_str(), "shm");
        assert_eq!(ClientBufferKind::Dmabuf.as_str(), "dmabuf");
    }
}
