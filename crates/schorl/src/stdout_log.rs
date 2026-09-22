//! 標準出力へ JSON 一行ずつ書く adapter。
//!
//! `pin code.log.format = json` の実体側。書式は [`schorl_core::log::LogRecord`]
//! が持っているので、ここは行き先だけを担う。

use std::io::Write as _;

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_core::log::{LogRecord, LogSink};

/// 標準出力へ出す行き先。
#[derive(Debug, Clone, Copy, Default)]
pub struct StdoutJsonLogSink;

impl LogSink for StdoutJsonLogSink {
    fn emit(&self, record: &LogRecord) -> Result<()> {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{}", record.to_line()).map_err(|e| {
            Error::new(
                ErrorCode::Internal,
                "failed to write a log line to stdout",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schorl_core::log::Level;
    use schorl_core::time::UtcTimestamp;

    #[test]
    fn writing_to_stdout_succeeds() {
        let record = LogRecord::new(
            UtcTimestamp::from_millis_since_epoch(0),
            Level::Debug,
            TraceId::unattributed(),
            "stdout sink smoke test",
        );
        StdoutJsonLogSink.emit(&record).expect("stdout is writable");
    }
}
