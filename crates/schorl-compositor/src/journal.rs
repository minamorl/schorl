//! ログの出し口と、uuidv7 による識別子の発行。
//!
//! `pin code.log.format = json` と `pin code.log.required_fields in [ts, level, trace_id, msg]`
//! は `schorl_core::log::LogRecord` が形で持っている。ここはその形へ「いま」と
//! 「どの追跡子か」を与えるだけの薄い層である。時刻もログの行き先も周囲効果なので、
//! 自分で掴まずに capability として受け取る (`house.effect_boundary.location`)。

use std::sync::Arc;

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::{Id, IdGen, IdScheme, TraceId};
use schorl_core::log::{Level, LogRecord, LogSink};
use schorl_core::time::Clock;

/// uuidv7 を発行する [`IdGen`]。
///
/// `pin code.id.scheme: require id.scheme in [uuidv7, ulid]` の uuidv7 側。
/// 綴りは `uuid` crate の `Uuid::now_v7()` が作る小文字ハイフン区切りをそのまま使う。
#[derive(Debug, Clone, Copy, Default)]
pub struct Uuidv7IdGen;

impl IdGen for Uuidv7IdGen {
    fn scheme(&self) -> IdScheme {
        IdScheme::Uuidv7
    }

    fn next_id(&self) -> Result<Id> {
        Id::new(IdScheme::Uuidv7, uuid::Uuid::now_v7().to_string())
    }
}

/// ログを出すための束。
///
/// 一つの起動に一つの追跡子を持たせ、その起動が出す全行へ同じ `trace_id` を焼く。
#[derive(Clone)]
pub struct Journal {
    clock: Arc<dyn Clock>,
    sink: Arc<dyn LogSink>,
    trace_id: TraceId,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("trace_id", &self.trace_id)
            .finish_non_exhaustive()
    }
}

impl Journal {
    /// 時刻・行き先・追跡子を束ねる。
    pub const fn new(clock: Arc<dyn Clock>, sink: Arc<dyn LogSink>, trace_id: TraceId) -> Self {
        Self {
            clock,
            sink,
            trace_id,
        }
    }

    /// 追跡子を新しく発行して束ねる。
    ///
    /// 発行に失敗したら封筒で返す。黙って `unattributed` へ落とさない。
    pub fn with_new_trace(
        clock: Arc<dyn Clock>,
        sink: Arc<dyn LogSink>,
        ids: &dyn IdGen,
    ) -> Result<Self> {
        let id = ids.next_id()?;
        Ok(Self::new(clock, sink, TraceId::new(id)))
    }

    /// この束が焼いている追跡子。封筒と突き合わせるために要る。
    pub const fn trace_id(&self) -> &TraceId {
        &self.trace_id
    }

    /// 一行出す。行き先の失敗は封筒で返る。
    pub fn emit(&self, level: Level, msg: impl Into<String>) -> Result<()> {
        let record = LogRecord::new(self.clock.now_utc(), level, self.trace_id.clone(), msg);
        self.sink.emit(&record)
    }

    /// 通常の進行。
    pub fn info(&self, msg: impl Into<String>) -> Result<()> {
        self.emit(Level::Info, msg)
    }

    /// 続行はできるが注意が要る。
    pub fn warn(&self, msg: impl Into<String>) -> Result<()> {
        self.emit(Level::Warn, msg)
    }

    /// 失敗。
    pub fn error(&self, msg: impl Into<String>) -> Result<()> {
        self.emit(Level::Error, msg)
    }

    /// 出せなかったときに握り潰さず、別の封筒へ畳んで返すための補助。
    ///
    /// ログが出ないこと自体は compositor を止める理由にならないが、
    /// 黙って消すのは `code.error.envelope` の趣旨に反する。呼ぶ側が
    /// 「止める」「無視する」を選べるように `Result` のまま返す。
    pub fn note_failure(&self, what: &str, cause: &Error) -> Result<()> {
        self.error(format!("{what}: {}", cause.message()))
    }
}

/// 標準エラー出力へ JSON を一行ずつ書く行き先。
///
/// compositor の標準出力はソケット名の受け渡しに使いたいので、ログは標準エラーへ出す。
#[derive(Debug, Clone, Copy, Default)]
pub struct StderrJsonLogSink;

impl LogSink for StderrJsonLogSink {
    fn emit(&self, record: &LogRecord) -> Result<()> {
        use std::io::Write as _;
        let mut out = std::io::stderr().lock();
        writeln!(out, "{}", record.to_line()).map_err(|e| {
            Error::new(
                ErrorCode::Internal,
                "failed to write a log line to stderr",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_core::testing::{CapturingLogSink, FixedClock};

    #[test]
    fn uuidv7_ids_are_distinct_and_carry_the_pinned_scheme() {
        let ids = Uuidv7IdGen;
        assert_eq!(ids.scheme(), IdScheme::Uuidv7);
        let a = ids.next_id().expect("generated");
        let b = ids.next_id().expect("generated");
        assert_eq!(a.scheme(), IdScheme::Uuidv7);
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(a.as_str().len(), 36, "hyphenated uuid is 36 characters");
    }

    #[test]
    fn every_line_carries_the_four_pinned_fields_and_the_same_trace_id() {
        let sink = Arc::new(CapturingLogSink::new());
        let journal = Journal::with_new_trace(
            Arc::new(FixedClock::at_millis(7)),
            sink.clone(),
            &Uuidv7IdGen,
        )
        .expect("trace id issued");

        journal.info("socket bound").expect("emitted");
        journal.warn("client gone").expect("emitted");

        let lines = sink.lines();
        assert_eq!(lines.len(), 2);
        let trace = journal.trace_id().as_str().expect("attributed").to_owned();
        for line in &lines {
            assert!(line.starts_with("{\"ts\":7,\"level\":"), "{line}");
            assert!(
                line.contains(&format!("\"trace_id\":\"{trace}\"")),
                "{line}"
            );
            assert!(line.contains("\"msg\":"), "{line}");
        }
    }
}
