//! 識別子。
//!
//! `pin code.id.scheme: require id.scheme in [uuidv7, ulid]` を型で固定する。
//! [`IdScheme`] は二つの枝しか持たないので、他の方式は表現できない。
//!
//! **生成そのものはこの phase では実装しない。** uuidv7 も ulid も外部仕様であり、
//! `P-EX-2` (記憶した API の形から書かない) に照らすと、一次資料を読める phase で
//! 埋めるのが正しい。ここでは [`IdGen`] という署名だけを置き、穴を型で表す。
//! panic する stub は置かない (`rust.error_boundary.no_panic`)。

use crate::error::{Error, ErrorCode, Result};

/// 許された ID 方式。この二つ以外は表現できない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdScheme {
    /// RFC 9562 の UUID version 7。
    Uuidv7,
    /// ULID。
    Ulid,
}

impl IdScheme {
    /// ログや封筒へ出すときの名前。
    pub const fn as_str(self) -> &'static str {
        match self {
            IdScheme::Uuidv7 => "uuidv7",
            IdScheme::Ulid => "ulid",
        }
    }
}

/// 生成済みの識別子。どの方式で作られたかを一緒に持ち歩く。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Id {
    scheme: IdScheme,
    text: String,
}

impl Id {
    /// 既に生成された文字列から作る。空文字だけを拒む。
    ///
    /// 方式ごとの綴りの検査はここではしない。綴りの規則は外部仕様であり、
    /// 一次資料を読まずに検査を書くと、間違った検査が緑になるだけである。
    pub fn new(scheme: IdScheme, text: impl Into<String>) -> Result<Self> {
        let text = text.into();
        if text.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "identifier text must not be empty",
                TraceId::unattributed(),
            )
            .with_detail("scheme", scheme.as_str()));
        }
        Ok(Self { scheme, text })
    }

    /// 方式。
    pub const fn scheme(&self) -> IdScheme {
        self.scheme
    }

    /// 文字列表現。
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

/// 識別子を発行する capability。
///
/// 乱数もクロックも実装側が閉じ込める。呼ぶ側は方式と結果だけを見る。
pub trait IdGen: Send + Sync {
    /// この実装が発行する方式。
    fn scheme(&self) -> IdScheme;

    /// 新しい識別子を発行する。
    fn next_id(&self) -> Result<Id>;
}

/// ログとエラー封筒を突き合わせるための追跡子。
///
/// `pin code.log.required_fields` と `pin code.error.envelope` の両方が要求している。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TraceId(Option<Id>);

impl TraceId {
    /// 識別子から作る。
    pub const fn new(id: Id) -> Self {
        Self(Some(id))
    }

    /// 追跡子をまだ発行できていない場面を表す。
    ///
    /// [`IdGen`] が未配線の phase でも封筒の形を崩さないために要る。JSON では `null`。
    pub const fn unattributed() -> Self {
        Self(None)
    }

    /// 文字列表現。未発行なら `None`。
    pub fn as_str(&self) -> Option<&str> {
        self.0.as_ref().map(Id::as_str)
    }
}

/// 副作用のある書き込みを重複させないための鍵。
///
/// `pin code.idempotency.write: require write.idempotency = required`。
/// 仮想出力の作成・ポインタ注入・キー注入のように外へ効く操作は、
/// すべてこの鍵を伴う型で受ける。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(Id);

impl IdempotencyKey {
    /// 識別子から作る。
    pub const fn new(id: Id) -> Self {
        Self(id)
    }

    /// 文字列表現。
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_identifier_text() {
        let err = Id::new(IdScheme::Ulid, "").expect_err("empty text must be rejected");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn keeps_scheme_with_the_identifier() {
        let id = Id::new(IdScheme::Uuidv7, "0199-fixture").expect("valid text");
        assert_eq!(id.scheme(), IdScheme::Uuidv7);
        assert_eq!(id.as_str(), "0199-fixture");
    }

    #[test]
    fn unattributed_trace_id_has_no_text() {
        assert_eq!(TraceId::unattributed().as_str(), None);
    }

    #[test]
    fn idempotency_key_exposes_its_text() {
        let key = IdempotencyKey::new(Id::new(IdScheme::Ulid, "key-1").expect("valid text"));
        assert_eq!(key.as_str(), "key-1");
    }
}
