//! `schorl-display` — 板の中身になる Linux 側の出力。
//!
//! 満たす pin:
//! - `v1.panel_source: require schorl.v1.panel.content_source = linux_display` —
//!   板の中身は [`OutputId`] が指す Linux の出力から来る。
//! - `host.created_output_lifecycle: require schorl.created_virtual_output.lifetime
//!   = released_at_process_exit` — [`VirtualOutput`] は所有権つきの手綱で、
//!   `Drop` でも明示解放でも必ず返る。
//! - `host.no_persistent_config_change: forbid schorl.host_compositor.persistent_configuration
//!   = modified` — 実装が申告する [`HostOp`] は [`assert_runtime_only`] で検める。
//! - `house.resource_lifecycle.*` — 取得・使用・解放を同じ scope に置き、
//!   成功・失敗・早期 return・巻き戻しのどれでも解放が走る。
//! - `code.idempotency.write` — 出力の作成は [`IdempotencyKey`] を伴う。
//!
//! どの仕掛けで出力を用意するか (`free schorl.display.provisioning_method`) は
//! ここでは決めない。決まっているのは「作ったら返す」「恒久設定は触らない」だけ。

pub mod hyprland;

use std::fmt;
use std::sync::Arc;

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::{IdempotencyKey, TraceId};

/// Linux 側の出力の名前。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutputId(String);

impl OutputId {
    /// 名前から作る。空名は拒む。
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "output id must not be empty",
                TraceId::unattributed(),
            ));
        }
        Ok(Self(name))
    }

    /// 名前。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OutputId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 出力を一枚借りる注文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRequest {
    /// 希望する名前 (ホストが別名を返すことはある)。
    pub preferred_name: String,
    /// 横の画素数。
    pub width_px: u32,
    /// 縦の画素数。
    pub height_px: u32,
    /// 更新頻度 (mHz)。`free schorl.panel.refresh_rate`。
    pub refresh_millihz: u32,
    /// 重複作成を防ぐ鍵 (`code.idempotency.write`)。
    pub idempotency_key: IdempotencyKey,
}

/// ホストへ加えた操作の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostOpKind {
    /// 実行時だけの操作。プロセスが消えれば残らない。
    Runtime,
    /// 恒久設定の書き換え。**pin により禁止**。値として在るのは、
    /// 実装が誤って申告したときに [`assert_runtime_only`] で落とすため。
    PersistentConfiguration,
}

/// ホストへ加えた操作の申告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOp {
    /// 種類。
    pub kind: HostOpKind,
    /// 何をしたか。
    pub description: String,
}

impl HostOp {
    /// 実行時操作として申告する。
    pub fn runtime(description: impl Into<String>) -> Self {
        Self {
            kind: HostOpKind::Runtime,
            description: description.into(),
        }
    }
}

/// 申告された操作がすべて実行時操作であることを確かめる。
///
/// 一つでも恒久設定の書き換えがあれば封筒で落とす
/// (`host.no_persistent_config_change`)。
pub fn assert_runtime_only(ops: &[HostOp]) -> Result<()> {
    for op in ops {
        if op.kind == HostOpKind::PersistentConfiguration {
            return Err(Error::new(
                ErrorCode::HostRefused,
                "schorl must not modify persistent host compositor configuration",
                TraceId::unattributed(),
            )
            .with_detail("operation", op.description.clone()));
        }
    }
    Ok(())
}

/// 借りた出力を返す口。
///
/// 実装は冪等であること。`Drop` と明示解放の両方から呼ばれ得る。
pub trait OutputRelease: Send + Sync {
    /// 出力を返す。
    fn release(&self, id: &OutputId) -> Result<()>;
}

/// 借りている出力の手綱。
///
/// `Clone` も `Copy` も付けない。写しが作れないので、所有権の移動だけが
/// scope から逃がす経路になる (`house.resource_lifecycle.explicit_escape`)。
pub struct VirtualOutput {
    id: OutputId,
    releaser: Option<Arc<dyn OutputRelease>>,
}

impl VirtualOutput {
    /// 手綱を作る。作った実装だけが呼ぶ。
    pub fn new(id: OutputId, releaser: Arc<dyn OutputRelease>) -> Self {
        Self {
            id,
            releaser: Some(releaser),
        }
    }

    /// 借りている出力。
    pub const fn id(&self) -> &OutputId {
        &self.id
    }

    /// 明示的に返す。失敗を報告できる唯一の経路。
    ///
    /// 呼ばずに落としても `Drop` が返す。こちらは結果を見たいときに使う。
    pub fn release(mut self) -> Result<()> {
        match self.releaser.take() {
            Some(releaser) => releaser.release(&self.id),
            None => Ok(()),
        }
    }
}

impl fmt::Debug for VirtualOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualOutput")
            .field("id", &self.id)
            .field("held", &self.releaser.is_some())
            .finish()
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        // 成功・失敗・早期 return・巻き戻しのどれでもここを通る
        // (`house.resource_lifecycle.release_paths`)。`Drop` は失敗を返せないので
        // 握り潰す。報告が要るなら [`VirtualOutput::release`] を使う。
        if let Some(releaser) = self.releaser.take() {
            let _ = releaser.release(&self.id);
        }
    }
}

/// 出力を一枚用意する capability。
///
/// 実装は「作って、返す」だけを担う。捕捉も入力も持たない
/// (`house.effect_boundary.no_god_capability`)。
pub trait VirtualOutputProvider: Send + Sync {
    /// 注文どおりの出力を借りる。
    fn create(&self, request: &OutputRequest) -> Result<VirtualOutput>;

    /// この実装がホストへ加える操作の申告。
    ///
    /// 呼ぶ側が [`assert_runtime_only`] に掛けられるようにするために在る。
    fn declared_host_ops(&self) -> Vec<HostOp>;
}

/// 試験用の差し替え実装 (`house.effect_boundary.substitution`)。
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    /// 解放の呼び出しを数える贋物。
    #[derive(Debug, Default)]
    pub struct RecordingReleaser {
        released: Mutex<Vec<String>>,
    }

    impl RecordingReleaser {
        /// 空で作る。
        pub fn new() -> Self {
            Self::default()
        }

        /// 返された出力の名前。
        pub fn released(&self) -> Vec<String> {
            match self.released.lock() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            }
        }
    }

    impl OutputRelease for RecordingReleaser {
        fn release(&self, id: &OutputId) -> Result<()> {
            match self.released.lock() {
                Ok(mut g) => g.push(id.as_str().to_owned()),
                Err(p) => p.into_inner().push(id.as_str().to_owned()),
            }
            Ok(())
        }
    }

    /// 何も作らずに手綱だけ返す贋物の提供者。
    #[derive(Debug)]
    pub struct FakeOutputProvider {
        releaser: Arc<RecordingReleaser>,
    }

    impl FakeOutputProvider {
        /// 記録用の解放器と一緒に作る。
        pub fn new(releaser: Arc<RecordingReleaser>) -> Self {
            Self { releaser }
        }
    }

    impl VirtualOutputProvider for FakeOutputProvider {
        fn create(&self, request: &OutputRequest) -> Result<VirtualOutput> {
            let id = OutputId::new(request.preferred_name.clone())?;
            Ok(VirtualOutput::new(id, self.releaser.clone()))
        }

        fn declared_host_ops(&self) -> Vec<HostOp> {
            vec![HostOp::runtime("create a headless output for the lifetime of the process")]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeOutputProvider, RecordingReleaser};
    use super::*;
    use schorl_core::id::{Id, IdScheme};

    fn request(name: &str) -> OutputRequest {
        OutputRequest {
            preferred_name: name.to_owned(),
            width_px: 1920,
            height_px: 1080,
            refresh_millihz: 60_000,
            idempotency_key: IdempotencyKey::new(
                Id::new(IdScheme::Ulid, "out-1").expect("valid text"),
            ),
        }
    }

    #[test]
    fn dropping_the_handle_returns_the_output() {
        let releaser = Arc::new(RecordingReleaser::new());
        let provider = FakeOutputProvider::new(releaser.clone());
        {
            let output = provider.create(&request("SCHORL-1")).expect("created");
            assert_eq!(output.id().as_str(), "SCHORL-1");
            assert!(releaser.released().is_empty(), "still held inside the scope");
        }
        assert_eq!(releaser.released(), vec!["SCHORL-1".to_owned()]);
    }

    #[test]
    fn an_early_return_still_returns_the_output() {
        let releaser = Arc::new(RecordingReleaser::new());
        let provider = FakeOutputProvider::new(releaser.clone());

        fn bail_out(provider: &dyn VirtualOutputProvider, req: &OutputRequest) -> Result<()> {
            let _output = provider.create(req)?;
            Err(Error::new(
                ErrorCode::Internal,
                "something went wrong after acquiring",
                TraceId::unattributed(),
            ))
        }

        let err = bail_out(&provider, &request("SCHORL-2")).expect_err("bails out");
        assert_eq!(err.code(), ErrorCode::Internal);
        assert_eq!(releaser.released(), vec!["SCHORL-2".to_owned()]);
    }

    #[test]
    fn explicit_release_happens_exactly_once() {
        let releaser = Arc::new(RecordingReleaser::new());
        let provider = FakeOutputProvider::new(releaser.clone());
        let output = provider.create(&request("SCHORL-3")).expect("created");
        output.release().expect("released");
        assert_eq!(releaser.released(), vec!["SCHORL-3".to_owned()]);
    }

    #[test]
    fn a_persistent_configuration_change_is_refused() {
        let ops = vec![
            HostOp::runtime("create a headless output"),
            HostOp {
                kind: HostOpKind::PersistentConfiguration,
                description: "rewrite hyprland.conf".to_owned(),
            },
        ];
        let err = assert_runtime_only(&ops).expect_err("persistent change must be refused");
        assert_eq!(err.code(), ErrorCode::HostRefused);
    }

    #[test]
    fn the_fake_provider_only_declares_runtime_operations() {
        let provider = FakeOutputProvider::new(Arc::new(RecordingReleaser::new()));
        assert_runtime_only(&provider.declared_host_ops()).expect("runtime only");
    }

    #[test]
    fn empty_output_names_are_refused() {
        assert_eq!(
            OutputId::new("").expect_err("empty name").code(),
            ErrorCode::InvalidArgument
        );
    }
}
