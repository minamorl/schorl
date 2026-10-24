//! schorl の入口。**v1 の製品は一本のプログラムである。**
//!
//! ここが起こすのは一つの座で、その中に
//!
//! - 自分の Wayland ソケットと seat を持つ compositor (`schorl-compositor`)、
//! - 黒い 360 度の空間へ矩形を描いて OpenXR の swapchain へ出す描画面
//!   (`schorl-render`)、
//! - コントローラで掴んで置き直す入口 (`schorl-panel` の算術 + OpenXR の action)
//!
//! が同時に入っている。繋ぎ目は [`schorl::session::SchorlSession`]。
//!
//! # 使い方
//!
//! ```text
//! schorl [client argv…]
//! ```
//!
//! `.desktop` file から作る launcher を最初の window として自動で起こす。引数を
//! 与えた場合は、その client も schorl の socket へ向けて起こす。宿主側には
//! PC input 用の小窓を一枚置き、その窓が focus 中だけ keyboard / mouse を VR 内の
//! 選択中 window へ渡す。F8 で対象を次へ送る。宿主の `WAYLAND_DISPLAY` は奪わない
//! (`pin wm.host_compositor_coexistence`)。
//!
//! `SCHORL_RUN_FOR_SECONDS` を与えるとその秒数で畳む。与えなければランタイムが
//! 終了を告げるまで回る。
//!
//! # 何を主張しないか
//!
//! 終了コードは「ビルドが通り、ここまで走った」以上のことを言わない。
//! HMD を被っての受け入れは御主人様の身体が要る
//! (`pin verify.hmd_gate` / `pin verify.no_green_substitute`)。最後に出す
//! `hmd_acceptance` の行は必ず `unknown` である。

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use schorl::StdoutJsonLogSink;
use schorl::host_input::HostInput;
use schorl::session::{SchorlSession, SessionOptions};
use schorl_core::error::Result;
use schorl_core::id::{Id, IdScheme, TraceId};
use schorl_core::log::{Level, LogRecord, LogSink};
use schorl_core::time::{Clock, SystemClock};
use schorl_verify::{
    CheckOutcome, HmdAcceptance, MachineCheck, MachineRun, hmd_acceptance_from_machine,
};
use schorl_xr::ThreadSleeper;

fn main() -> ExitCode {
    let clock = SystemClock;
    let sink: Arc<dyn LogSink> = Arc::new(StdoutJsonLogSink);
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // 走らせる前に言えるのは一つだけ。残りはこの走りが埋める。
    let mut run = MachineRun::new().record(MachineCheck::BuildPasses, CheckOutcome::Green);

    match serve(&argv, Arc::clone(&sink), &clock) {
        Ok(observed) => {
            run = run
                .record(MachineCheck::OpenxrSessionOpens, CheckOutcome::Green)
                .record(
                    MachineCheck::ClientFrameReachesSwapchain,
                    if observed.client_pixels_drawn {
                        CheckOutcome::Green
                    } else {
                        CheckOutcome::NotRun
                    },
                );
        }
        Err(e) => {
            emit(
                sink.as_ref(),
                &clock,
                Level::Error,
                &format!("schorl stopped: {}", e.envelope().to_json().render()),
            );
            run = run.record(MachineCheck::OpenxrSessionOpens, CheckOutcome::Red);
            report(sink.as_ref(), &clock, &run);
            return ExitCode::FAILURE;
        }
    }

    report(sink.as_ref(), &clock, &run);
    ExitCode::SUCCESS
}

/// この走りで観測できたこと。
#[derive(Debug, Default)]
struct Observed {
    client_pixels_drawn: bool,
}

fn serve(argv: &[String], sink: Arc<dyn LogSink>, clock: &dyn Clock) -> Result<Observed> {
    let mut host_input = HostInput::open()?;
    let mut session = SchorlSession::open(SessionOptions::default(), Arc::clone(&sink))?;
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "connect clients with WAYLAND_DISPLAY={}",
            session.socket_name()
        ),
    );

    let mut children = vec![session.spawn_launcher()?];
    if let Some(child) = session.spawn_client(argv)? {
        children.push(child);
    }
    let mut observed = Observed::default();

    if !session.wait_until_running(&ThreadSleeper)? {
        emit(
            sink.as_ref(),
            clock,
            Level::Warn,
            "the OpenXR session never reached RUNNING; no frame was submitted",
        );
    } else {
        let limit = std::env::var("SCHORL_RUN_FOR_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let started = Instant::now();
        let input_started = Instant::now();
        loop {
            let time_ms = u32::try_from(input_started.elapsed().as_millis() % u128::from(u32::MAX))
                .unwrap_or(0);
            let mut close_requested = false;
            for event in host_input.poll()? {
                close_requested |= session.apply_host_input(event, time_ms)?;
            }
            let outcome = session.step(false)?;
            if !session.stage().is_empty() {
                observed.client_pixels_drawn = true;
            }
            if outcome.exiting || close_requested {
                break;
            }
            if limit.is_some_and(|l| started.elapsed() >= l) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "render ledger: {}",
            session.xr().render_facts().to_json().render()
        ),
    );
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
    // 起こした物はここで返す (`pin host.created_resource_lifecycle`)。
    session.close()?;
    Ok(observed)
}

/// 五つの機械検査の現状と、人間ゲートの答えを出す。
fn report(sink: &dyn LogSink, clock: &dyn Clock, run: &MachineRun) {
    for (check, outcome) in run.entries() {
        let level = match outcome {
            CheckOutcome::Green => Level::Info,
            CheckOutcome::NotRun => Level::Warn,
            CheckOutcome::Red => Level::Error,
        };
        emit(
            sink,
            clock,
            level,
            &format!("machine check {} is {}", check.as_str(), outcome.as_str()),
        );
    }
    let msg = match hmd_acceptance_from_machine(run) {
        HmdAcceptance::Unknown => {
            "hmd acceptance is unknown: it needs a human wearing the quest 3, not a machine green"
        }
        HmdAcceptance::AcceptedByHumanWearingQuest3 { .. } => {
            "hmd acceptance was recorded by a human wearing the quest 3"
        }
    };
    emit(sink, clock, Level::Warn, msg);
}

fn emit(sink: &dyn LogSink, clock: &dyn Clock, level: Level, message: &str) {
    // 走りの中の行は compositor の [`Journal`] が自分の追跡子を焼く。ここは
    // その外 (起動前と畳んだあと) で出る行なので、この面の綴りを焼いておく。
    // 空欄にしないのは `pin code.log.required_fields` が trace_id を数えるため。
    let trace = Id::new(IdScheme::Ulid, "01JSCHORLBOUNDARY00000000")
        .map(TraceId::new)
        .unwrap_or_else(|_| TraceId::unattributed());
    let record = LogRecord::new(clock.now_utc(), level, trace, message);
    let _ = sink.emit(&record);
}
