//! ログ。
//!
//! 満たす pin:
//! - `code.log.format: require log.format = json`
//! - `code.log.required_fields: require log.fields in [ts, level, trace_id, msg]`
//! - `code.log.no_secrets: forbid log.contains = secret`
//!
//! [`LogRecord`] は四つの欄しか持たない。自由な付加欄が無いので、秘密が紛れ込む
//! 経路は `msg` だけに絞られる。[`crate::secret::Secret`] は `Debug`/`Display` で
//! 中身を出さないので、うっかり `format!` しても綴りは出ない。

use crate::error::Result;
use crate::id::TraceId;
use crate::json::JsonValue;
use crate::time::UtcTimestamp;

/// ログの深刻度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// 開発時の細目。
    Debug,
    /// 通常の進行。
    Info,
    /// 続行はできるが注意が要る。
    Warn,
    /// 失敗。
    Error,
}

impl Level {
    /// JSON に出す綴り。
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// 一行分のログ。pin が許した四つの欄だけを持つ。
#[derive(Debug, Clone)]
pub struct LogRecord {
    ts: UtcTimestamp,
    level: Level,
    trace_id: TraceId,
    msg: String,
}

impl LogRecord {
    /// 四つの欄を与えて作る。
    pub fn new(ts: UtcTimestamp, level: Level, trace_id: TraceId, msg: impl Into<String>) -> Self {
        Self {
            ts,
            level,
            trace_id,
            msg: msg.into(),
        }
    }

    /// 時刻 (UTC epoch ミリ秒)。
    pub const fn ts(&self) -> UtcTimestamp {
        self.ts
    }

    /// 深刻度。
    pub const fn level(&self) -> Level {
        self.level
    }

    /// 追跡子。
    pub const fn trace_id(&self) -> &TraceId {
        &self.trace_id
    }

    /// 本文。
    pub fn msg(&self) -> &str {
        &self.msg
    }

    /// JSON 表現。欄と順序はここで固定する。
    pub fn to_json(&self) -> JsonValue {
        JsonValue::Object(vec![
            ("ts".to_owned(), JsonValue::Int(self.ts.millis_since_epoch())),
            ("level".to_owned(), JsonValue::text(self.level.as_str())),
            (
                "trace_id".to_owned(),
                match self.trace_id.as_str() {
                    Some(t) => JsonValue::text(t),
                    None => JsonValue::Null,
                },
            ),
            ("msg".to_owned(), JsonValue::text(self.msg.clone())),
        ])
    }

    /// 一行の JSON テキスト。
    pub fn to_line(&self) -> String {
        self.to_json().render()
    }
}

/// ログの行き先。書き出しは周囲効果なので capability にしてある。
pub trait LogSink: Send + Sync {
    /// 一行出す。出力先の失敗は封筒で返し、panic しない。
    fn emit(&self, record: &LogRecord) -> Result<()>;
}

/// 共有された行き先も行き先として扱える。
///
/// 組み立ての境界が所有したまま、試験が同じ sink を覗けるようにするために要る。
impl<T: LogSink + ?Sized> LogSink for std::sync::Arc<T> {
    fn emit(&self, record: &LogRecord) -> Result<()> {
        (**self).emit(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{Id, IdScheme};

    #[test]
    fn line_has_exactly_the_four_pinned_fields_in_order() {
        let record = LogRecord::new(
            UtcTimestamp::from_millis_since_epoch(17),
            Level::Info,
            TraceId::new(Id::new(IdScheme::Ulid, "t").expect("valid text")),
            "hello",
        );
        assert_eq!(
            record.to_line(),
            "{\"ts\":17,\"level\":\"info\",\"trace_id\":\"t\",\"msg\":\"hello\"}"
        );
    }

    #[test]
    fn unattributed_trace_id_renders_as_null() {
        let record = LogRecord::new(
            UtcTimestamp::from_millis_since_epoch(0),
            Level::Warn,
            TraceId::unattributed(),
            "no trace yet",
        );
        assert!(record.to_line().contains("\"trace_id\":null"));
    }
}
