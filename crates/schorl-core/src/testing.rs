//! 試験用の差し替え実装。
//!
//! `house.effect_boundary.substitution: require ... = multiple_implementations_or_test_seam`
//! を満たすための継ぎ目。ここに在る実装は決定的で、外の世界に触らない。
//! 他 crate の試験からも使えるように `#[cfg(test)]` では隠していない。

use crate::error::Result;
use crate::id::{Id, IdGen, IdScheme, TraceId};
use crate::log::{LogRecord, LogSink};
use crate::time::{Clock, UtcTimestamp};
use std::sync::Mutex;

/// 止まった時計。
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(UtcTimestamp);

impl FixedClock {
    /// epoch ミリ秒で止める。
    pub const fn at_millis(millis: i64) -> Self {
        Self(UtcTimestamp::from_millis_since_epoch(millis))
    }
}

impl Clock for FixedClock {
    fn now_utc(&self) -> UtcTimestamp {
        self.0
    }
}

/// 連番の識別子を返す発行器。
///
/// 綴りは試験用の固定形式であり、uuidv7 でも ulid でもない。方式の申告だけを
/// 本物に合わせてある (綴りの生成は一次資料を読む phase の仕事)。
#[derive(Debug)]
pub struct SequentialIdGen {
    scheme: IdScheme,
    prefix: String,
    next: Mutex<u64>,
}

impl SequentialIdGen {
    /// 方式と接頭辞を決めて作る。
    pub fn new(scheme: IdScheme, prefix: impl Into<String>) -> Self {
        Self {
            scheme,
            prefix: prefix.into(),
            next: Mutex::new(0),
        }
    }
}

impl IdGen for SequentialIdGen {
    fn scheme(&self) -> IdScheme {
        self.scheme
    }

    fn next_id(&self) -> Result<Id> {
        let n = match self.next.lock() {
            Ok(mut guard) => {
                let n = *guard;
                *guard = guard.saturating_add(1);
                n
            }
            // 毒された錠でも panic しない。
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                let n = *guard;
                *guard = guard.saturating_add(1);
                n
            }
        };
        Id::new(self.scheme, format!("{}-{n:08}", self.prefix))
    }
}

/// 追跡子を一つ作る補助。試験の準備を短くするためだけに在る。
pub fn trace_id(text: &str) -> Result<TraceId> {
    Ok(TraceId::new(Id::new(IdScheme::Ulid, text)?))
}

/// 出した行を溜めるログ行き先。
#[derive(Debug, Default)]
pub struct CapturingLogSink {
    lines: Mutex<Vec<String>>,
}

impl CapturingLogSink {
    /// 空で作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// 溜まった行の写し。
    pub fn lines(&self) -> Vec<String> {
        match self.lines.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl LogSink for CapturingLogSink {
    fn emit(&self, record: &LogRecord) -> Result<()> {
        let line = record.to_line();
        match self.lines.lock() {
            Ok(mut guard) => guard.push(line),
            Err(poisoned) => poisoned.into_inner().push(line),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Level;

    #[test]
    fn fixed_clock_does_not_move() {
        let clock = FixedClock::at_millis(42);
        assert_eq!(clock.now_utc().millis_since_epoch(), 42);
        assert_eq!(clock.now_utc().millis_since_epoch(), 42);
    }

    #[test]
    fn sequential_ids_differ_and_keep_the_scheme() {
        let ids = SequentialIdGen::new(IdScheme::Uuidv7, "test");
        let a = ids.next_id().expect("id");
        let b = ids.next_id().expect("id");
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(a.scheme(), IdScheme::Uuidv7);
        assert_eq!(ids.scheme(), IdScheme::Uuidv7);
    }

    #[test]
    fn capturing_sink_keeps_the_rendered_line() {
        let sink = CapturingLogSink::new();
        let record = LogRecord::new(
            FixedClock::at_millis(1).now_utc(),
            Level::Info,
            trace_id("t").expect("trace"),
            "hi",
        );
        sink.emit(&record).expect("capturing sink never fails");
        assert_eq!(sink.lines().len(), 1);
        assert!(sink.lines()[0].contains("\"msg\":\"hi\""));
    }
}
