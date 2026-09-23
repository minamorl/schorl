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

/// 本物のランタイムから来た手の姿勢が、板平面への射影を通って Linux 側の
/// ポインタ事象になるところまでを一本で見る。
///
/// 貼る面は無いので絵は流れない (`presents_frames` が偽)。流れるのは位置だけ。
#[test]
#[ignore = "needs a running OpenXR runtime (e.g. monado-service); run with --ignored"]
fn real_controller_poses_drive_the_panel_cursor() {
    let _serial = hold_the_runtime();

    use schorl_capture::testing::SolidColourFrameSource;
    use schorl_core::id::IdScheme;
    use schorl_core::testing::{FixedClock, SequentialIdGen};
    use schorl_core::time::UtcTimestamp;
    use schorl_display::OutputId;
    use schorl_input::PointerEventKind;
    use schorl_input::testing::RecordingSink;
    use schorl_panel::cursor::CursorResolution;
    use schorl_xr::driver::{DriverConfig, PanelDriver, PanelWiring};
    use schorl_xr::sleep::testing::CountingSleeper;

    let runtime = HeadlessRuntime::with_thread_sleeper(HeadlessConfig::new());
    let config = SessionConfig::for_panel(Panel::default_single());
    let (mut session, _facts) = runtime
        .open_headless_session(&config)
        .expect("a headless session opens");

    let clock = FixedClock::at_millis(1_700_000_000_000);
    let ids = SequentialIdGen::new(IdScheme::Ulid, "live");
    let mut frames = SolidColourFrameSource {
        width_px: 1920,
        height_px: 1080,
        fill: 0,
        captured_at: UtcTimestamp::from_millis_since_epoch(0),
    };
    let mut pointer = RecordingSink::new();
    let mut keyboard = RecordingSink::new();
    let sleeper = CountingSleeper::new();
    let output = OutputId::new("SCHORL-PANEL").expect("valid name");

    let mut seen_pose = false;
    let mut cursor_seen = None;
    {
        let mut driver = PanelDriver::new(
            PanelWiring {
                clock: &clock,
                ids: &ids,
                frames: &mut frames,
                pointer: &mut pointer,
                keyboard: &mut keyboard,
                sleeper: &sleeper,
            },
            DriverConfig::DEFAULT,
            Panel::default_single(),
            output.clone(),
        );

        for _ in 0..40 {
            let outcome = driver.step(&mut session).expect("a live step");
            assert!(
                outcome.plan.is_none(),
                "a headless session has no surface, so nothing is pasted"
            );
            if let Some(cursor) = outcome.cursor {
                seen_pose = true;
                cursor_seen = Some(cursor);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        println!("panel after live stepping = {:?}", driver.panel().pose());
    }

    assert!(
        seen_pose,
        "the live runtime must report at least one controller pose"
    );
    let cursor = cursor_seen.expect("a cursor was resolved");
    println!("cursor from the live runtime = {cursor:?}");

    let delivered = pointer.pointer_events();
    println!("pointer events delivered = {}", delivered.len());
    if let Some(first) = delivered.first() {
        println!("first pointer event = {:?}", first.kind);
    }
    match cursor {
        CursorResolution::OnPanel(_) | CursorResolution::BeyondEdgeWhileHeld(_) => {
            assert!(
                !delivered.is_empty(),
                "a resolved cursor position must reach the Linux side"
            );
            match &delivered[0].kind {
                PointerEventKind::MotionAbsolute { output: named, .. } => {
                    assert_eq!(named.as_str(), "SCHORL-PANEL");
                }
                PointerEventKind::Button { .. } => {
                    unreachable!("nothing pressed a button in this run")
                }
            }
        }
        CursorResolution::OffPanel => {
            // 手が板の外にあり押してもいないなら、動きは出ない。それも正しい枝。
            assert!(
                delivered.is_empty(),
                "an off-panel cursor with no hold must move nothing"
            );
        }
    }

    session.end().expect("the session closes");
}
