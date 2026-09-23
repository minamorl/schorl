//! エラー封筒。
//!
//! 満たす pin:
//! - `code.error.envelope: require error.envelope = { code, message, details, trace_id }`
//! - `code.error.http_shape: require error.body.field in [code, message, details]`
//! - `code.error.no_raw_stack: forbid error.expose = raw_stacktrace`
//! - `rust.error_boundary.*` (回復可能な公開操作は `Result`、原因は保つ、panic しない)
//!
//! 生のスタックトレースを出せないことは型で担保する。[`DetailValue`] に
//! バックトレースを入れる枝が無く、[`ErrorEnvelope`] は原因鎖を描画しない。
//! 原因は [`std::error::Error::source`] から辿れるが、封筒には出ない。

use crate::id::TraceId;
use crate::json::JsonValue;
use std::error::Error as StdError;
use std::fmt;

/// 機械が分岐するための分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// 引数が契約を満たしていない。
    InvalidArgument,
    /// この構成では扱えない要求 (範囲外の機能など)。
    Unsupported,
    /// 必要な capability がまだ配線されていない。
    CapabilityUnavailable,
    /// ホスト側 (compositor / ランタイム) が拒んだ。
    HostRefused,
    /// 資源の解放に失敗した。
    ResourceReleaseFailed,
    /// 上のどれでもない内部の失敗。
    Internal,
}

impl ErrorCode {
    /// 封筒に出す安定した綴り。
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidArgument => "invalid_argument",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::CapabilityUnavailable => "capability_unavailable",
            ErrorCode::HostRefused => "host_refused",
            ErrorCode::ResourceReleaseFailed => "resource_release_failed",
            ErrorCode::Internal => "internal",
        }
    }
}

/// `details` に入れてよい値。
///
/// 文字列・整数・真偽値だけ。スタックトレースを入れる枝が無いことが
/// `code.error.no_raw_stack` の担保である。
#[derive(Debug, Clone, PartialEq)]
pub enum DetailValue {
    /// 文字列。秘密を入れてはならない (`code.log.no_secrets` と同じ理由)。
    Text(String),
    /// 整数。
    Int(i64),
    /// 真偽値。
    Bool(bool),
}

impl From<&str> for DetailValue {
    fn from(value: &str) -> Self {
        DetailValue::Text(value.to_owned())
    }
}

impl From<String> for DetailValue {
    fn from(value: String) -> Self {
        DetailValue::Text(value)
    }
}

impl From<i64> for DetailValue {
    fn from(value: i64) -> Self {
        DetailValue::Int(value)
    }
}

impl From<bool> for DetailValue {
    fn from(value: bool) -> Self {
        DetailValue::Bool(value)
    }
}

impl DetailValue {
    fn to_json(&self) -> JsonValue {
        match self {
            DetailValue::Text(s) => JsonValue::text(s.clone()),
            DetailValue::Int(n) => JsonValue::Int(*n),
            DetailValue::Bool(b) => JsonValue::Bool(*b),
        }
    }
}

/// 封筒の `details`。挿入順を保つ小さな連想列。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Details(Vec<(String, DetailValue)>);

impl Details {
    /// 空の `details`。
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// 項目を足す。同じ鍵が既にあれば置き換える。
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<DetailValue>) {
        let key = key.into();
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }

    /// 項目の走査。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &DetailValue)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// 項目数。
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 空か。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn to_json(&self) -> JsonValue {
        JsonValue::Object(
            self.0
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect(),
        )
    }
}

/// schorl の回復可能な失敗。
///
/// 公開する失敗はすべてこの型で返す (`rust.error_boundary.result` /
/// `.explicit_error`)。原因は `source` に保つので文脈は失われない
/// (`rust.error_boundary.wrapping`)。
#[derive(Debug)]
pub struct Error {
    code: ErrorCode,
    message: String,
    details: Details,
    trace_id: TraceId,
    source: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl Error {
    /// 封筒の四つの欄のうち三つを与えて作る。`details` は後から足す。
    pub fn new(code: ErrorCode, message: impl Into<String>, trace_id: TraceId) -> Self {
        Self {
            code,
            message: message.into(),
            details: Details::new(),
            trace_id,
            source: None,
        }
    }

    /// `details` に一項目足す。
    #[must_use]
    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<DetailValue>) -> Self {
        self.details.insert(key, value);
        self
    }

    /// 原因を包む。文脈を保つための経路。
    #[must_use]
    pub fn caused_by(mut self, source: impl StdError + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// 分類。
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    /// 人が読む説明。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 付随情報。
    pub const fn details(&self) -> &Details {
        &self.details
    }

    /// 追跡子。
    pub const fn trace_id(&self) -> &TraceId {
        &self.trace_id
    }

    /// 封筒への射影。
    pub fn envelope(&self) -> ErrorEnvelope<'_> {
        ErrorEnvelope {
            code: self.code,
            message: &self.message,
            details: &self.details,
            trace_id: &self.trace_id,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 原因鎖もスタックも出さない。出すのは分類と説明だけ。
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn StdError + 'static))
    }
}

/// `pin code.error.envelope` が定める四つの欄。
#[derive(Debug, Clone, Copy)]
pub struct ErrorEnvelope<'a> {
    /// 分類。
    pub code: ErrorCode,
    /// 人が読む説明。
    pub message: &'a str,
    /// 付随情報。
    pub details: &'a Details,
    /// 追跡子。
    pub trace_id: &'a TraceId,
}

impl ErrorEnvelope<'_> {
    /// 四つの欄すべてを持つ JSON。内部の記録・ログ相関用。
    pub fn to_json(&self) -> JsonValue {
        JsonValue::Object(vec![
            ("code".to_owned(), JsonValue::text(self.code.as_str())),
            ("message".to_owned(), JsonValue::text(self.message)),
            ("details".to_owned(), self.details.to_json()),
            (
                "trace_id".to_owned(),
                match self.trace_id.as_str() {
                    Some(t) => JsonValue::text(t),
                    None => JsonValue::Null,
                },
            ),
        ])
    }

    /// 外へ返す body。
    ///
    /// `pin code.error.http_shape: require error.body.field in [code, message, details]`
    /// により、`trace_id` は body に出さない。スタックも当然出さない。
    pub fn to_body_json(&self) -> JsonValue {
        JsonValue::Object(vec![
            ("code".to_owned(), JsonValue::text(self.code.as_str())),
            ("message".to_owned(), JsonValue::text(self.message)),
            ("details".to_owned(), self.details.to_json()),
        ])
    }
}

/// この workspace の既定の `Result`。
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{Id, IdScheme};

    fn trace() -> TraceId {
        TraceId::new(Id::new(IdScheme::Ulid, "trace-1").expect("valid text"))
    }

    #[test]
    fn envelope_has_exactly_the_four_pinned_fields() {
        let err = Error::new(ErrorCode::HostRefused, "compositor said no", trace())
            .with_detail("protocol", "virtual_output");
        let JsonValue::Object(fields) = err.envelope().to_json() else {
            unreachable!("envelope renders as an object")
        };
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["code", "message", "details", "trace_id"]);
    }

    #[test]
    fn body_omits_trace_id_and_carries_only_three_fields() {
        let err = Error::new(ErrorCode::Internal, "boom", trace());
        let JsonValue::Object(fields) = err.envelope().to_body_json() else {
            unreachable!("body renders as an object")
        };
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["code", "message", "details"]);
    }

    #[test]
    fn display_shows_no_cause_chain() {
        let inner = std::io::Error::other("inner detail that must not leak");
        let err = Error::new(ErrorCode::Internal, "outer", trace()).caused_by(inner);
        let shown = err.to_string();
        assert_eq!(shown, "internal: outer");
        assert!(err.source().is_some(), "cause is still reachable in code");
    }

    #[test]
    fn details_replace_on_duplicate_key() {
        let mut details = Details::new();
        details.insert("k", 1_i64);
        details.insert("k", 2_i64);
        assert_eq!(details.len(), 1);
        assert_eq!(details.iter().next(), Some(("k", &DetailValue::Int(2))));
    }
}
