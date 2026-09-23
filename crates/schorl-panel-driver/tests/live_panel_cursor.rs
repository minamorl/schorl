//! `schorl-panel-driver` の loop を、本物の OpenXR ランタイムに対して一本で回す検査。
//!
//! **この経路は spec 0.2 の原文3 で v1 から外れている。** 残してあるのは実測資産
//! だからで、緑になっても v1 の到達点を一つも主張しない。走っているランタイムが
//! 要るので `#[ignore]` のまま (この `#[ignore]` は `schorl-xr` から一緒に運んできた
//! もので、新しく足したものではない)。走らせ方:
//!
//! ```text
//! cargo test -p schorl-panel-driver --test live_panel_cursor -- --ignored --nocapture
//! ```

use std::sync::{Mutex, MutexGuard};

use schorl_panel::panel::Panel;
use schorl_xr::openxr_runtime::{HeadlessConfig, HeadlessRuntime};
use schorl_xr::{SessionConfig, XrSession};

/// 一つのプロセスで `XrInstance` を同時に二つ持てない。実測でローダがそう言う:
/// 「Error [GENERAL | xrCreateInstance | OpenXR-Loader] : Loader does not support
/// simultaneous XrInstances」。試験どうしは直列に走らせる。
static RUNTIME_LOCK: Mutex<()> = Mutex::new(());

fn hold_the_runtime() -> MutexGuard<'static, ()> {
    // 前の試験が落ちて毒が付いていても、直列化の目的は果たせる。
    RUNTIME_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
    use schorl_panel_driver::{DriverConfig, PanelDriver, PanelWiring};
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
                PointerEventKind::MotionAbsolute { x_px, y_px } => {
                    // 0.2 以前はここで出力名 "SCHORL-PANEL" を見ていた。その欄は
                    // 原文3 で退役したので、残った不変条件 (板の画素範囲に入る位置を
                    // 運ぶこと) を見る。
                    let res = Panel::default_single().resolution();
                    assert!(*x_px >= 0 && (*x_px as u32) < res.width_px, "x_px = {x_px}");
                    assert!(*y_px >= 0 && (*y_px as u32) < res.height_px, "y_px = {y_px}");
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
