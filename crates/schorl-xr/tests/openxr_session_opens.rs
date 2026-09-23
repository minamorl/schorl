//! `pin verify.machine_scope` の `openxr_session_opens` を、本物のランタイムに対して
//! 確かめる検査。
//!
//! 走っている OpenXR ランタイムが要るので `#[ignore]` を付けてある。既定の
//! `cargo test --workspace` はランタイム不在のホストでも緑になり、**この検査が
//! 走らなかったことは `ignored` として出力に残る**。走らせ方:
//!
//! ```text
//! cargo test -p schorl-xr --test openxr_session_opens -- --ignored --nocapture
//! ```
//!
//! **これが緑でも実機の受け入れにはならない。** 被って確かめるのは人の身体の
//! 仕事である (`verify.hmd_gate` / `verify.no_green_substitute`)。ここで言えるのは
//! 「グラフィクス束縛なしでセッションが開き、状態が進み、束縛が受け取られた」まで。

use std::sync::{Mutex, MutexGuard};

use schorl_panel::panel::Panel;
use schorl_verify::{CheckOutcome, HmdAcceptance, MachineCheck, MachineRun, hmd_acceptance_from_machine};
use schorl_xr::openxr_runtime::{HeadlessConfig, HeadlessRuntime, ReferenceSpaceChoice};
use schorl_xr::{SessionConfig, SessionKind, XrSession};
use schorl_scope::Background;

/// 一つのプロセスで `XrInstance` を同時に二つ持てない。実測でローダがそう言う:
/// 「Error [GENERAL | xrCreateInstance | OpenXR-Loader] : Loader does not support
/// simultaneous XrInstances」。試験どうしは直列に走らせる。
static RUNTIME_LOCK: Mutex<()> = Mutex::new(());

fn hold_the_runtime() -> MutexGuard<'static, ()> {
    // 前の試験が落ちて毒が付いていても、直列化の目的は果たせる。
    RUNTIME_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
#[ignore = "needs a running OpenXR runtime (e.g. monado-service); run with --ignored"]
fn a_headless_session_opens_against_the_active_runtime() {
    let _serial = hold_the_runtime();
    let runtime = HeadlessRuntime::with_thread_sleeper(HeadlessConfig::new());
    let config = SessionConfig::for_panel(Panel::default_single());
    assert_eq!(config.kind, SessionKind::Full);
    assert_eq!(config.background, Background::Black);

    let (mut session, facts) = runtime
        .open_headless_session(&config)
        .expect("a headless session opens against the active runtime");

    println!("runtime_name            = {}", facts.runtime_name);
    println!("runtime_version         = {}", facts.runtime_version);
    println!(
        "advertised_extensions   = {}",
        facts.advertised_extension_count
    );
    println!("headless_advertised     = {}", facts.headless_advertised);
    println!(
        "timespec_time_advertised = {}",
        facts.timespec_time_advertised
    );
    println!("reference_space          = {}", facts.reference_space.as_str());
    println!("bound_profiles           = {:?}", facts.bound_profiles);
    println!("states_before_begin      = {:?}", facts.observed_states);

    assert!(facts.headless_advertised, "XR_MND_headless must be present");
    assert!(
        !facts.bound_profiles.is_empty(),
        "at least one interaction profile must accept the panel bindings"
    );
    assert!(
        matches!(
            facts.reference_space,
            ReferenceSpaceChoice::Stage | ReferenceSpaceChoice::Local
        ),
        "the panel must live in a world-locked space"
    );
    assert!(
        facts
            .observed_states
            .iter()
            .any(|state| state == "ready"),
        "the session must have reached READY: {:?}",
        facts.observed_states
    );

    // 合成面が無いので絵は貼れない。貼れないことを型と署名で言っている。
    assert!(
        !session.presents_frames(),
        "a headless session has no composition surface"
    );

    // 何巡か回して、状態が進むことと落ちないことを見る。
    let mut polls = 0usize;
    let mut events = 0usize;
    for _ in 0..50 {
        let batch = session.poll_events().expect("polling a live session");
        polls += 1;
        events += batch.len();
        for event in &batch {
            println!("event = {event:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // 束縛の提案が通ったことと、入力源が本当に束ねられたことは別。数える。
    // **セッションが走り出してから聞かないと空が返る** (実測: open 直後に聞くと
    // 両手とも空だった。interaction profile が有効になるのは begin のあと)。
    let bound = session.bound_sources().expect("bound sources can be listed");
    println!("bound grab  = {:?}", bound.grab);
    println!("bound click = {:?}", bound.click);
    println!("bound aim   = {:?}", bound.aim);
    assert!(
        bound.all_bound(),
        "grab, click and aim must each be bound to a real input source: {bound:?}"
    );

    println!("polls = {polls}, events = {events}");
    println!("observed_states_after = {:?}", session.observed_states());

    session.end().expect("the session closes");

    // 緑をここまでにする。実機の受け入れは動かない。
    let run = MachineRun::new()
        .record(MachineCheck::BuildPasses, CheckOutcome::Green)
        .record(MachineCheck::OpenxrSessionOpens, CheckOutcome::Green)
        .record(MachineCheck::UnitTests, CheckOutcome::Green);
    assert_eq!(
        hmd_acceptance_from_machine(&run),
        HmdAcceptance::Unknown,
        "machine green is never hmd acceptance"
    );
}
