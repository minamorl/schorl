//! schorl の入口。
//!
//! この phase では、機械が確かめられる五つの検査のうち何が済んでいるかを
//! そのまま述べるだけにしてある。捕捉も注入も OpenXR も未配線なので、
//! できていないことを緑と言わない (`verify.no_green_substitute`)。
//!
//! 出す行は `pin code.log.required_fields` の四欄 JSON。終了コードは、
//! 「ビルドが通ってここまで来た」以上のことを主張しない。

use schorl::StdoutJsonLogSink;
use schorl_core::log::{Level, LogRecord, LogSink};
use schorl_core::id::TraceId;
use schorl_core::time::{Clock, SystemClock};
use schorl_verify::{CheckOutcome, HmdAcceptance, MachineCheck, MachineRun, hmd_acceptance_from_machine};
use std::process::ExitCode;

fn main() -> ExitCode {
    let clock = SystemClock;
    let sink = StdoutJsonLogSink;

    // ここまで走っている事実だけが根拠になる。他の四つは走らせていない。
    let run = MachineRun::new().record(MachineCheck::BuildPasses, CheckOutcome::Green);

    for (check, outcome) in run.entries() {
        let line = LogRecord::new(
            clock.now_utc(),
            match outcome {
                CheckOutcome::Green => Level::Info,
                CheckOutcome::NotRun => Level::Warn,
                CheckOutcome::Red => Level::Error,
            },
            TraceId::unattributed(),
            format!("machine check {} is {}", check.as_str(), outcome.as_str()),
        );
        if sink.emit(&line).is_err() {
            return ExitCode::FAILURE;
        }
    }

    let acceptance = hmd_acceptance_from_machine(&run);
    let msg = match acceptance {
        HmdAcceptance::Unknown => {
            "hmd acceptance is unknown: it needs a human wearing the quest 3, not a machine green"
        }
        HmdAcceptance::AcceptedByHumanWearingQuest3 { .. } => {
            "hmd acceptance was recorded by a human wearing the quest 3"
        }
    };
    let line = LogRecord::new(clock.now_utc(), Level::Warn, TraceId::unattributed(), msg);
    if sink.emit(&line).is_err() {
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
