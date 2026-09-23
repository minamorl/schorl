//! schorl を入れ子 backend で起こす実行ファイル。
//!
//! 既存の Wayland セッションの上に窓を一枚もらい、その中で schorl 自身の
//! compositor を回す。schorl は自分のソケットを取るので、宿主の compositor は
//! そのまま動き続ける (`pin wm.host_compositor_coexistence`)。
//!
//! ```text
//! schorl-nested --run-for 8 --client foot
//! ```
//!
//! `--client` を付けると、そのコマンドを `WAYLAND_DISPLAY=<schorl のソケット>`
//! で起こす。自分の環境変数は書き換えない。

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use schorl_compositor::journal::{Journal, StderrJsonLogSink, Uuidv7IdGen};
use schorl_compositor::nested::{NestedOptions, run};
use schorl_compositor::state::CompositorSetup;
use schorl_core::time::SystemClock;

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `pin code.error.envelope` の四欄をそのまま JSON で出す。
            eprintln!("{}", e.envelope().to_json().render());
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> schorl_core::error::Result<()> {
    let mut options = NestedOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--run-for" => {
                let secs = args
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| bad_argument("--run-for wants a whole number of seconds"))?;
                options.run_for = Some(Duration::from_secs(secs));
            }
            "--scale" => {
                let scale = args
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .ok_or_else(|| bad_argument("--scale wants a number"))?;
                options.view_scale = scale;
            }
            "--title" => {
                options.title = args
                    .next()
                    .ok_or_else(|| bad_argument("--title wants a string"))?;
            }
            "--client" => {
                options.client = args.by_ref().collect();
                break;
            }
            other => return Err(bad_argument(&format!("unknown argument {other}"))),
        }
    }

    let ids = Arc::new(Uuidv7IdGen);
    let journal = Journal::with_new_trace(
        Arc::new(SystemClock),
        Arc::new(StderrJsonLogSink),
        ids.as_ref(),
    )?;
    let setup = CompositorSetup::new(ids, journal);

    let outcome = run(setup, options)?;
    // 標準出力には結果だけを出す。ログは標準エラーへ行っている。
    println!(
        "{{\"socket\":\"{}\",\"frames\":{},\"clients_accepted\":{},\"toplevels_seen\":{},\"mapped_windows\":{}}}",
        outcome.socket,
        outcome.frames,
        outcome.clients_accepted,
        outcome.toplevels_seen,
        outcome.mapped_windows
    );
    Ok(())
}

fn bad_argument(message: &str) -> schorl_core::error::Error {
    schorl_core::error::Error::new(
        schorl_core::error::ErrorCode::InvalidArgument,
        message.to_owned(),
        schorl_core::id::TraceId::unattributed(),
    )
}
