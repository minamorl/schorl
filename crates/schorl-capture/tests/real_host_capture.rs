//! 実ホストで本当にフレームが返るかを確かめる検査。
//!
//! 0.1 の `verify.machine_scope` の `capture_returns_real_frame` を、贋物ではなく
//! 実物で埋めるためのものだった。**spec 0.2 の原文3 でその項目は
//! `client_frame_reaches_swapchain` へ改鍵され、この経路は v1 から外れた。**
//! ここで測っているのはホストの画面であってクライアントの一枚ではないので、
//! この検査は 0.2 の五項目をどれも埋めない (下を見よ)。
//! **これは機械の緑であって受け入れではない**
//! (`verify.no_green_substitute` / `property verify.green_is_not_acceptance`)。
//! この検査が緑でも Quest 3 の受け入れは `unknown` のままである。
//!
//! 走らせ方 (環境が要るので既定では走らない):
//!
//! ```text
//! SCHORL_REAL_HOST=1 WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/1000 \
//!   cargo test -p schorl-capture --test real_host_capture -- --nocapture
//! ```
//!
//! `SCHORL_REAL_HOST` が無いときは黙って緑にせず、飛ばしたことを標準エラーへ
//! 書いてから抜ける。compositor の無い機械で `cargo test --workspace` を
//! 赤にしないためだが、飛ばしたことは隠さない。

use std::env;
use std::sync::Arc;
use std::time::Duration;

use schorl_capture::FrameSource;
use schorl_capture::screencopy::{ScreencopyFrameSource, capture_with_retry};
use schorl_core::id::{Id, IdScheme, IdempotencyKey};
use schorl_core::retry::{Backoff, RetryPolicy};
use schorl_core::time::SystemClock;
use schorl_display::hyprland::{
    CommandRunner, HyprlandHeadlessOutputProvider, SystemCommandRunner, parse_instance_signature,
    parse_monitor_names,
};
use schorl_display::{OutputRequest, VirtualOutputProvider, assert_runtime_only};
use schorl_verify::{CheckOutcome, MachineCheck, MachineRun, hmd_acceptance_from_machine};

const OPT_IN: &str = "SCHORL_REAL_HOST";
const PANEL_WIDTH: u32 = 1280;
const PANEL_HEIGHT: u32 = 720;
const PANEL_REFRESH_MILLIHZ: u32 = 60_000;
/// 捕捉前に共有メモリを埋める色。返った絵がこの色一色なら compositor は書いていない。
const POISON: u8 = 0xA5;

#[test]
fn the_host_returns_a_real_frame_from_an_output_schorl_created_and_removed() {
    if env::var_os(OPT_IN).is_none() {
        eprintln!(
            "SKIPPED: {OPT_IN} is not set, so this check did not touch a compositor. \
             nothing was measured."
        );
        return;
    }

    let wayland_display =
        env::var("WAYLAND_DISPLAY").expect("WAYLAND_DISPLAY must point at the host compositor");
    let runtime_dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR must be set");

    // hyprctl は instance signature を要る。socket から引き当てる。
    let bare = SystemCommandRunner::new();
    let instances = bare
        .run("hyprctl", &["-j", "instances"])
        .expect("hyprctl must be runnable");
    assert!(instances.is_success(), "hyprctl -j instances failed");
    let signature = parse_instance_signature(&instances.stdout, &wayland_display)
        .unwrap_or_else(|| panic!("no hyprland instance owns socket {wayland_display}"));

    let runner: Arc<dyn CommandRunner> = Arc::new(
        SystemCommandRunner::new()
            .with_env("HYPRLAND_INSTANCE_SIGNATURE", &signature)
            .with_env("WAYLAND_DISPLAY", &wayland_display)
            .with_env("XDG_RUNTIME_DIR", &runtime_dir),
    );
    let provider = HyprlandHeadlessOutputProvider::new(runner.clone());

    // 申告がすべて実行時操作であること (`host.no_persistent_config_change`)。
    assert_runtime_only(&provider.declared_host_ops())
        .expect("the hyprland provider must only declare runtime operations");

    let monitors_before = list_monitors(runner.as_ref());
    eprintln!("monitors before: {monitors_before:?}");
    assert!(
        !monitors_before.is_empty(),
        "the host must already have an output; this check must not be the only one"
    );

    let request = OutputRequest {
        preferred_name: "SCHORL-PANEL".to_owned(),
        width_px: PANEL_WIDTH,
        height_px: PANEL_HEIGHT,
        refresh_millihz: PANEL_REFRESH_MILLIHZ,
        idempotency_key: IdempotencyKey::new(
            Id::new(IdScheme::Ulid, "real-host-capture-1").expect("valid text"),
        ),
    };

    let created_name;
    let run = MachineRun::new();
    {
        let output = provider
            .create(&request)
            .expect("hyprland must lend a headless output");
        created_name = output.id().as_str().to_owned();
        eprintln!("created output: {created_name}");
        assert!(
            !monitors_before.contains(&created_name),
            "schorl must not claim an output that already existed"
        );

        let mut source = ScreencopyFrameSource::connect(Arc::new(SystemClock))
            .expect("the compositor must accept a screencopy client")
            .with_timeout(Duration::from_secs(5))
            // 検出器の較正: compositor が一画素も書かなければ毒の色のまま返る。
            .with_poison_fill(Some(POISON));

        let policy = RetryPolicy::new(
            5,
            Backoff::ExponentialJitter {
                base_millis: 50,
                cap_millis: 500,
            },
        )
        .expect("inside the pinned range 0..5");

        let names = source.output_names().expect("the compositor lists outputs");
        eprintln!("outputs seen over wayland: {names:?}");
        assert!(
            names.contains(&created_name),
            "the created output must be visible over wayland"
        );

        let frame = capture_with_retry(
            &mut source as &mut dyn FrameSource,
            output.id(),
            policy,
            &mut || 0.5,
            &mut std::thread::sleep,
        )
        .expect("the compositor must return a frame from the output schorl created");

        assert!(
            frame.is_real_capture(),
            "a frame that is not a real capture must never satisfy this check"
        );
        assert_eq!(frame.width_px(), PANEL_WIDTH);
        assert_eq!(frame.height_px(), PANEL_HEIGHT);
        assert_eq!(
            frame.pixels().len(),
            frame.stride_bytes() as usize * frame.height_px() as usize
        );
        assert!(
            frame.captured_at().millis_since_epoch() > 0,
            "the frame carries a UTC timestamp, not the protocol's monotonic one"
        );

        assert!(
            frame.pixels().iter().any(|byte| *byte != POISON),
            "every byte is still the poison value, so the compositor never wrote the buffer"
        );

        let distinct = distinct_byte_count(frame.pixels());
        let poisoned_bytes = frame.pixels().iter().filter(|b| **b == POISON).count();
        eprintln!(
            "frame: {}x{} stride={} format={:?} bytes={} distinct_byte_values={} bytes_still_poisoned={} at_utc_millis={}",
            frame.width_px(),
            frame.height_px(),
            frame.stride_bytes(),
            frame.format(),
            frame.pixels().len(),
            distinct,
            poisoned_bytes,
            frame.captured_at().millis_since_epoch(),
        );

        // 0.1 ではここで `MachineCheck::CaptureReturnsRealFrame` を緑にしていた。
        // 0.2 にその枝は無い。残った `client_frame_reaches_swapchain` を緑にするのは
        // **誤り**である — ここに client も swapchain も出てこないので、埋めたら
        // 測っていないものを緑と言うことになる。だから五項目はどれも NotRun のまま。
        // ここで `output` が落ちる。`Drop` が hyprctl output remove を呼ぶ。
    }

    let monitors_after = list_monitors(runner.as_ref());
    eprintln!("monitors after: {monitors_after:?}");
    assert!(
        !monitors_after.contains(&created_name),
        "the output schorl created must be gone once the handle is dropped \
         (host.created_output_lifecycle)"
    );
    assert_eq!(
        monitors_after, monitors_before,
        "outputs owned by other programs must be untouched"
    );

    // v1 の五項目はどれも埋まっていない。埋まっていないことを型で言う。
    for check in MachineCheck::ALL {
        assert_eq!(
            run.outcome(check),
            CheckOutcome::NotRun,
            "a retired-path capture fills none of the spec 0.2 machine checks: {}",
            check.as_str()
        );
    }
    // そして機械がどうであれ、実機受け入れは unknown のまま。
    eprintln!(
        "machine: spec 0.2 checks all not_run (retired path); hmd_acceptance={:?} \
         (verify.no_green_substitute)",
        hmd_acceptance_from_machine(&run)
    );
}

fn list_monitors(runner: &dyn CommandRunner) -> Vec<String> {
    let listed = runner
        .run("hyprctl", &["monitors"])
        .expect("hyprctl must be runnable");
    assert!(listed.is_success(), "hyprctl monitors failed");
    parse_monitor_names(&listed.stdout)
}

fn distinct_byte_count(pixels: &[u8]) -> usize {
    let mut seen = [false; 256];
    for byte in pixels {
        seen[*byte as usize] = true;
    }
    seen.iter().filter(|s| **s).count()
}
