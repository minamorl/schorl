//! HMD 無しで、繋ぎ目そのものを実測する。
//!
//! 測るのは二つ。どちらも「繋がっていない状態でも緑になってしまう検査」に
//! ならないよう、較正を同じ走りの中に置いてある。
//!
//! # 1. クライアントの画素が swapchain まで乗ったか
//!
//! `pin verify.machine_scope` の `client_frame_reaches_swapchain`。
//!
//! - 外のプロセス (`schorl-probe-client`) が 840 通りから並びを一つ選び、
//!   **描く前に**標準出力へ申告する。
//! - 検証側は申告を読み、swapchain の view 0 を読み戻して並びを照合する。
//! - 較正 (a): クライアントが繋がる前の一枚を同じ照合器へ通す。**通らない**。
//! - 較正 (b): 申告と違う並びを同じ関数で描き、同じ照合器へ通す。
//!   **その違う並びが返る**。よって照合器は入力で答えを変える。
//!
//! # 3. 同じことが dmabuf 経路でも起きるか
//!
//! `free schorl.compositor.buffer_import_path` は片道にしない構えである。上の
//! 1 が通るのは `wl_shm` の経路だけなので、もう一本 (`zwp_linux_dmabuf_v1`) を
//! 同じ厳しさで測る。
//!
//! - shm のクライアントを降ろし、較正 (c): 誰も居ない一枚で照合器が何も
//!   見つけないことを見る。**次に読めた並びが残像でないこと**がこれで言える。
//! - 別のプロセス (`schorl-probe-client-dmabuf`) が並びを選んで描く前に申告し、
//!   GPU 上の `VkImage` を dmabuf として渡す。**`wl_shm` を bind しないので
//!   退路が無い。**
//! - 読み戻した絵が申告どおりであることに加えて、台帳の `dmabuf_draws` が
//!   増え、`shm_draws` が一つも増えていないことを見る。
//!
//! # 2. コントローラで toplevel を掴んで置き直せたか
//!
//! `pin v1.window_grab`。実在のクライアントが出した toplevel に対して、
//! `schorl-panel` の掴みの算術を通し、台帳の姿勢が動き、**読み戻した絵の中で
//! 板が動いた**ことまでを見る。
//!
//! 掴みの入口 (action set) がランタイムに実際に束ねられたことは
//! `xrEnumerateBoundSourcesForAction` の生の答えで示す。
//! **ただし、この走りでボタンを押したのは人ではない。** 押下と手の姿勢は
//! 検証側が組み立てた [`XrEvent`] であり、実機のコントローラを握っての確認は
//! 御主人様の身体が要る (`pin verify.hmd_gate` / `pin verify.no_green_substitute`)。
//! その区別は下の `synthetic_controller_events` 欄で必ず申告する。

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use schorl::probe::{
    ColourCensus, MatchThresholds, PureColour, QuadPattern, census_bgra, paint_quadrants,
    read_pattern,
};
use schorl::session::{SchorlSession, SessionOptions, StepOutcome};
use schorl::{GrabEffect, StdoutJsonLogSink};
use schorl_compositor::window::{RingPlacement, WindowId};
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::{Id, IdScheme, TraceId};
use schorl_core::json::JsonValue;
use schorl_core::log::{Level, LogRecord, LogSink};
use schorl_core::time::{Clock, SystemClock};
use schorl_panel::grab::ControllerId;
use schorl_render::facts::TextureRoute;
use schorl_panel::math::{Pose, Quat, Vec3};
use schorl_xr::{PressState, ThreadSleeper, XrEvent};

/// shm でバッファを出すクライアント。
const SHM_PROBE: &str = "schorl-probe-client";
/// そのクライアントが申告に使う綴り。
const SHM_ANNOUNCEMENT: &str = "schorl-probe-client pattern=";
/// dmabuf でバッファを出すクライアント。
const DMABUF_PROBE: &str = "schorl-probe-client-dmabuf";
/// そのクライアントが申告に使う綴り。
const DMABUF_ANNOUNCEMENT: &str = "schorl-probe-client-dmabuf pattern=";

/// 掴んで動かす距離 (メートル)。`free` な軸の中の選択。
const GRAB_SHIFT_M: f32 = 0.30;
/// 板の重心がこれだけ画素動いていれば「絵の中でも動いた」と数える。
const CENTROID_SHIFT_MIN_PX: f64 = 12.0;

fn main() -> std::process::ExitCode {
    let sink = Arc::new(StdoutJsonLogSink);
    let clock = SystemClock;
    match run(Arc::clone(&sink), &clock) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            emit(
                sink.as_ref(),
                &clock,
                Level::Error,
                &format!("seam check failed: {}", e.envelope().to_json().render()),
            );
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(sink: Arc<StdoutJsonLogSink>, clock: &dyn Clock) -> Result<()> {
    let options = SessionOptions {
        application_name: "schorl-seam-check".to_owned(),
        // 板は目の高さに立てる。`free schorl.window.placement_policy` の中の選択で、
        // STAGE の原点は床なので 0 のままだと足元に出る。
        placement: RingPlacement {
            eye_height_m: 1.5,
            ..RingPlacement::DEFAULT
        },
        ..SessionOptions::default()
    };
    let mut session = SchorlSession::open(options, sink.clone())?;
    let socket = session.socket_name().to_owned();
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "schorl is one program now: {}",
            JsonValue::Object(vec![
                ("wayland_socket".into(), JsonValue::text(socket.clone())),
                (
                    "openxr_runtime".into(),
                    JsonValue::text(session.xr().facts().runtime_name.clone())
                ),
                (
                    "hand_input_wired".into(),
                    JsonValue::Bool(session.hands().is_some())
                ),
            ])
            .render()
        ),
    );

    if !session.wait_until_running(&ThreadSleeper)? {
        session.close()?;
        return Err(Error::new(
            ErrorCode::HostRefused,
            "the OpenXR session never reached RUNNING, so nothing could be measured",
            TraceId::unattributed(),
        ));
    }

    let extent = session.xr().extent();
    let width = extent.width;

    // --- 較正 (a): クライアントが繋がる前の一枚 ------------------------------
    let empty = capture(&mut session, 90)?;
    let empty_census = census_bgra(&empty, width);
    let empty_read = read_pattern(&empty_census, MatchThresholds::DEFAULT);
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "calibration a — the frame before any client: {}",
            JsonValue::Object(vec![
                (
                    "non_black_pixels".into(),
                    JsonValue::Int(empty_census.non_black_pixels as i64)
                ),
                (
                    "total_pixels".into(),
                    JsonValue::Int(empty_census.total_pixels as i64)
                ),
                (
                    "read_pattern".into(),
                    JsonValue::text(format!("{empty_read:?}"))
                ),
            ])
            .render()
        ),
    );
    if empty_read.is_ok() {
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "the reader found a client pattern in a frame drawn before any client existed",
            TraceId::unattributed(),
        ));
    }

    // --- クライアントを一本立てて、申告を読む -------------------------------
    let mut child = spawn_probe(SHM_PROBE, &socket)?;
    let announced = read_announcement(&mut child, SHM_ANNOUNCEMENT)?;
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "the client announced its own picture before drawing it: {}",
            JsonValue::Object(vec![
                ("pattern".into(), JsonValue::text(announced.to_letters())),
                (
                    "possible_arrangements".into(),
                    JsonValue::Int(QuadPattern::all_distinct().len() as i64)
                ),
            ])
            .render()
        ),
    );

    let mapped = session.pump_until(
        Duration::from_secs(25),
        Duration::from_millis(2),
        |s| !s.stage().is_empty(),
    )?;
    if !mapped {
        stop(&mut child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::HostRefused,
            "the probe client never put a window on the stage",
            TraceId::unattributed(),
        ));
    }

    let window = session
        .placements()
        .first()
        .map(|p| p.id.clone())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "a window is staged but the registry lists none",
                TraceId::unattributed(),
            )
        })?;
    let route = session.stage().route_of(&window);

    // --- 本題: 読み戻した絵の中に、クライアントの申告どおりの並びが在るか ----
    let with_client = capture(&mut session, 120)?;
    let census = census_bgra(&with_client, width);
    let read = read_pattern(&census, MatchThresholds::DEFAULT);
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "read back the image handed to the runtime: {}",
            JsonValue::Object(vec![
                ("window".into(), JsonValue::text(window.as_str().to_owned())),
                (
                    "texture_route".into(),
                    JsonValue::text(route.map_or("none", |r| r.as_str()).to_owned())
                ),
                ("announced".into(), JsonValue::text(announced.to_letters())),
                ("read".into(), JsonValue::text(format!("{read:?}"))),
                ("counts".into(), counts_json(&census)),
                (
                    "non_black_pixels".into(),
                    JsonValue::Int(census.non_black_pixels as i64)
                ),
            ])
            .render()
        ),
    );
    let read = match read {
        Ok(pattern) => pattern,
        Err(miss) => {
            stop(&mut child);
            session.close()?;
            return Err(Error::new(
                ErrorCode::Internal,
                "no client quartering was found in the image handed to the runtime",
                TraceId::unattributed(),
            )
            .with_detail("miss", format!("{miss:?}")));
        }
    };
    if read != announced {
        stop(&mut child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "the picture on the swapchain is not the one the client announced",
            TraceId::unattributed(),
        )
        .with_detail("announced", announced.to_letters())
        .with_detail("read", read.to_letters()));
    }

    // --- 較正 (b): 同じ照合器へ、申告と違う並びを通す ------------------------
    let decoy = QuadPattern::all_distinct()
        .into_iter()
        .find(|p| *p != announced)
        .ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "there is no arrangement other than the announced one",
                TraceId::unattributed(),
            )
        })?;
    let painted = paint_quadrants(decoy, 256, 256);
    let decoy_read = read_pattern(&census_bgra(&painted, 256), MatchThresholds::DEFAULT);
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "calibration b — the same reader on a decoy: {}",
            JsonValue::Object(vec![
                ("decoy".into(), JsonValue::text(decoy.to_letters())),
                ("read".into(), JsonValue::text(format!("{decoy_read:?}"))),
            ])
            .render()
        ),
    );
    if decoy_read != Ok(decoy) {
        stop(&mut child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "the reader did not follow a decoy, so it is not measuring the picture",
            TraceId::unattributed(),
        )
        .with_detail("decoy", decoy.to_letters())
        .with_detail("read", format!("{decoy_read:?}")));
    }

    // --- 掴んで置き直す -----------------------------------------------------
    let grab = measure_grab(&mut session, &window)?;
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "grabbed the toplevel with a controller pose and put it back down: {}",
            grab.to_json().render()
        ),
    );

    let after_move = capture(&mut session, 120)?;
    let moved_census = census_bgra(&after_move, width);
    let moved_read = read_pattern(&moved_census, MatchThresholds::DEFAULT);
    let before_centroid = pattern_centroid(&census, announced);
    let after_centroid = pattern_centroid(&moved_census, announced);
    let shift = match (before_centroid, after_centroid) {
        (Some(before), Some(after)) => Some(after.0 - before.0),
        _ => None,
    };
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "the picture moved in the submitted frame too: {}",
            JsonValue::Object(vec![
                (
                    "centroid_x_before_px".into(),
                    JsonValue::Int(before_centroid.map_or(-1.0, |c| c.0) as i64)
                ),
                (
                    "centroid_x_after_px".into(),
                    JsonValue::Int(after_centroid.map_or(-1.0, |c| c.0) as i64)
                ),
                (
                    "shift_px".into(),
                    JsonValue::Int(shift.unwrap_or(0.0) as i64)
                ),
                (
                    "still_the_announced_pattern".into(),
                    JsonValue::Bool(moved_read == Ok(announced))
                ),
            ])
            .render()
        ),
    );

    let grab_shown_in_the_image = shift.is_some_and(|s| s >= CENTROID_SHIFT_MIN_PX);
    if !grab_shown_in_the_image {
        stop(&mut child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "the window moved in the registry but not in the image handed to the runtime",
            TraceId::unattributed(),
        )
        .with_detail("shift_px", shift.unwrap_or(0.0) as i64));
    }
    if moved_read != Ok(announced) {
        stop(&mut child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "after the grab the picture is no longer the one the client announced",
            TraceId::unattributed(),
        ));
    }

    // 人が目で見られる形でも残す。**これは受け入れではない。**
    if let Ok(path) = std::env::var("SCHORL_SEAM_DUMP") {
        let _ = write_ppm(&path, &after_move, extent.width, extent.height);
        emit(
            sink.as_ref(),
            clock,
            Level::Info,
            &format!("wrote the submitted image to {path} (not an acceptance)"),
        );
    }

    // 掴みの入口がランタイムに束ねられているか。`xrEnumerateBoundSourcesForAction`
    // は interaction profile が決まってからでないと空を返すので、実際に
    // `xrSyncActions` を何度も通したあとのここで聞く。生の答えをそのまま貼る。
    if let Some(hands) = session.hands() {
        let bound = hands.bound_sources(session.xr())?;
        emit(
            sink.as_ref(),
            clock,
            Level::Info,
            &format!(
                "the runtime bound the grab entry: {}",
                JsonValue::Object(vec![
                    (
                        "profiles".into(),
                        JsonValue::Array(
                            hands
                                .bound_profiles()
                                .iter()
                                .map(|p| JsonValue::text(p.clone()))
                                .collect()
                        )
                    ),
                    ("grab".into(), spell(&bound.grab)),
                    ("click".into(), spell(&bound.click)),
                    ("aim".into(), spell(&bound.aim)),
                    ("all_bound".into(), JsonValue::Bool(bound.all_bound())),
                ])
                .render()
            ),
        );
    }

    // --- 3. 同じ一本通しを dmabuf で ----------------------------------------
    //
    // ここまでで通ったのは shm 経路だけである (`route` が `shm`)。
    // `free schorl.compositor.buffer_import_path` は片道にしない構えなので、
    // もう一方でも**同じ厳しさで**測る: クライアントが描く前に並びを申告し、
    // それを swapchain から読み戻して照合する。
    let before = session.xr().render_facts().clone();

    // shm のクライアントを先に降ろす。二枚同時に立つと、view 全体を数える
    // 照合器がどちらの並びを読んだのか言えなくなる。
    stop(&mut child);
    let cleared = session.pump_until(
        Duration::from_secs(15),
        Duration::from_millis(2),
        |s| s.stage().is_empty(),
    )?;
    if !cleared {
        session.close()?;
        return Err(Error::new(
            ErrorCode::HostRefused,
            "the shm client went away but its window never left the stage",
            TraceId::unattributed(),
        ));
    }

    // 較正 (c): クライアントが降りたあとの一枚。**照合器はもう何も見つけない。**
    // これが在るので、次に読めた並びは「前の走りの残像」ではありえない。
    let vacated = capture(&mut session, 90)?;
    let vacated_census = census_bgra(&vacated, width);
    let vacated_read = read_pattern(&vacated_census, MatchThresholds::DEFAULT);
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "calibration c — the frame after the shm client left: {}",
            JsonValue::Object(vec![
                (
                    "non_black_pixels".into(),
                    JsonValue::Int(vacated_census.non_black_pixels as i64)
                ),
                (
                    "read_pattern".into(),
                    JsonValue::text(format!("{vacated_read:?}"))
                ),
            ])
            .render()
        ),
    );
    if vacated_read.is_ok() {
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "the reader still found a client pattern after the client had gone",
            TraceId::unattributed(),
        ));
    }

    let mut dmabuf_child = spawn_probe(DMABUF_PROBE, &socket)?;
    let dmabuf_announced = read_announcement(&mut dmabuf_child, DMABUF_ANNOUNCEMENT)?;
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "the dmabuf client announced its own picture before drawing it: {}",
            JsonValue::Object(vec![
                (
                    "pattern".into(),
                    JsonValue::text(dmabuf_announced.to_letters())
                ),
                (
                    "same_as_the_shm_client".into(),
                    JsonValue::Bool(dmabuf_announced == announced)
                ),
            ])
            .render()
        ),
    );

    let dmabuf_mapped = session.pump_until(
        Duration::from_secs(30),
        Duration::from_millis(2),
        |s| !s.stage().is_empty(),
    )?;
    if !dmabuf_mapped {
        stop(&mut dmabuf_child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::HostRefused,
            "the dmabuf probe client never put a window on the stage",
            TraceId::unattributed(),
        ));
    }

    let dmabuf_window = session
        .placements()
        .first()
        .map(|p| p.id.clone())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "a window is staged but the registry lists none",
                TraceId::unattributed(),
            )
        })?;
    let dmabuf_route = session.stage().route_of(&dmabuf_window);
    if dmabuf_route != Some(TextureRoute::Dmabuf) {
        stop(&mut dmabuf_child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "the dmabuf probe client's texture did not come through the dma_buf route",
            TraceId::unattributed(),
        )
        .with_detail("route", dmabuf_route.map_or("none", |r| r.as_str())));
    }

    let with_dmabuf = capture(&mut session, 120)?;
    let dmabuf_census = census_bgra(&with_dmabuf, width);
    let dmabuf_read = read_pattern(&dmabuf_census, MatchThresholds::DEFAULT);
    let after = session.xr().render_facts().clone();
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "read back the dmabuf client's image from the swapchain: {}",
            JsonValue::Object(vec![
                (
                    "window".into(),
                    JsonValue::text(dmabuf_window.as_str().to_owned())
                ),
                (
                    "texture_route".into(),
                    JsonValue::text(dmabuf_route.map_or("none", |r| r.as_str()).to_owned())
                ),
                (
                    "announced".into(),
                    JsonValue::text(dmabuf_announced.to_letters())
                ),
                ("read".into(), JsonValue::text(format!("{dmabuf_read:?}"))),
                ("counts".into(), counts_json(&dmabuf_census)),
                (
                    "dmabuf_draws_before".into(),
                    JsonValue::Int(before.dmabuf_draws as i64)
                ),
                (
                    "dmabuf_draws_after".into(),
                    JsonValue::Int(after.dmabuf_draws as i64)
                ),
                (
                    "shm_draws_before".into(),
                    JsonValue::Int(before.shm_draws as i64)
                ),
                (
                    "shm_draws_after".into(),
                    JsonValue::Int(after.shm_draws as i64)
                ),
            ])
            .render()
        ),
    );
    let dmabuf_read = match dmabuf_read {
        Ok(pattern) => pattern,
        Err(miss) => {
            stop(&mut dmabuf_child);
            session.close()?;
            return Err(Error::new(
                ErrorCode::Internal,
                "no client quartering was found in the image after the dmabuf client drew",
                TraceId::unattributed(),
            )
            .with_detail("miss", format!("{miss:?}")));
        }
    };
    if dmabuf_read != dmabuf_announced {
        stop(&mut dmabuf_child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "the picture on the swapchain is not the one the dmabuf client announced",
            TraceId::unattributed(),
        )
        .with_detail("announced", dmabuf_announced.to_letters())
        .with_detail("read", dmabuf_read.to_letters()));
    }
    // 経路の申告だけでは足りない。**帳面の側でも dmabuf が増えている**こと、
    // そして shm が一つも増えていないことを見る。増えていたら、この phase の
    // 絵は退路から来たことになる。
    if after.dmabuf_draws <= before.dmabuf_draws {
        stop(&mut dmabuf_child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "the ledger counted no dma_buf draw while the dmabuf client's picture was up",
            TraceId::unattributed(),
        )
        .with_detail("dmabuf_draws", after.dmabuf_draws as i64));
    }
    if after.shm_draws != before.shm_draws {
        stop(&mut dmabuf_child);
        session.close()?;
        return Err(Error::new(
            ErrorCode::Internal,
            "the shm route kept drawing during the dmabuf phase, so the picture may be the fallback",
            TraceId::unattributed(),
        )
        .with_detail("shm_draws_before", before.shm_draws as i64)
        .with_detail("shm_draws_after", after.shm_draws as i64));
    }

    let render_facts = session.xr().render_facts().to_json();
    let poses_located = session.hands().map_or(0, |h| h.poses_located());
    let real_grab_changes = session.hands().map_or(0, |h| h.grab_changes());
    let accepted = session.pixels().accepted();
    let refusals = session.pixels().refusals();

    stop(&mut dmabuf_child);
    session.close()?;

    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!("render ledger: {}", render_facts.render()),
    );
    emit(
        sink.as_ref(),
        clock,
        Level::Info,
        &format!(
            "what this run does and does not claim: {}",
            JsonValue::Object(vec![
                (
                    "client_frame_reaches_swapchain".into(),
                    JsonValue::Bool(true)
                ),
                (
                    "measured_buffer_routes".into(),
                    JsonValue::Array(vec![
                        JsonValue::text(TextureRoute::Shm.as_str().to_owned()),
                        JsonValue::text(TextureRoute::Dmabuf.as_str().to_owned()),
                    ])
                ),
                (
                    "client_buffers_accepted".into(),
                    JsonValue::Int(accepted as i64)
                ),
                (
                    "client_buffers_refused".into(),
                    JsonValue::Int(refusals.len() as i64)
                ),
                (
                    "real_controller_poses_located".into(),
                    JsonValue::Int(poses_located as i64)
                ),
                (
                    "real_controller_grab_presses".into(),
                    JsonValue::Int(real_grab_changes as i64)
                ),
                (
                    "synthetic_controller_events".into(),
                    JsonValue::Bool(true)
                ),
                (
                    "note".into(),
                    JsonValue::text(
                        "the grab button in this run was pressed by the check, not by a hand; \
                         wearing the Quest 3 is a human gate and no machine green stands in for it"
                    )
                ),
                ("hmd_accepted".into(), JsonValue::text("unknown")),
            ])
            .render()
        ),
    );
    Ok(())
}

/// 掴んで動かして離した記録。
#[derive(Debug)]
struct GrabMeasurement {
    window: String,
    before: Vec3,
    after: Vec3,
    wanted_shift_m: f32,
    effects: Vec<String>,
    still_there_after_release: bool,
}

impl GrabMeasurement {
    fn to_json(&self) -> JsonValue {
        let mm = |v: f32| JsonValue::Int((v * 1000.0) as i64);
        JsonValue::Object(vec![
            ("window".into(), JsonValue::text(self.window.clone())),
            (
                "before_mm".into(),
                JsonValue::Array(vec![mm(self.before.x), mm(self.before.y), mm(self.before.z)]),
            ),
            (
                "after_mm".into(),
                JsonValue::Array(vec![mm(self.after.x), mm(self.after.y), mm(self.after.z)]),
            ),
            ("wanted_shift_mm".into(), mm(self.wanted_shift_m)),
            (
                "effects".into(),
                JsonValue::Array(
                    self.effects
                        .iter()
                        .map(|e| JsonValue::text(e.clone()))
                        .collect(),
                ),
            ),
            (
                "still_there_after_release".into(),
                JsonValue::Bool(self.still_there_after_release),
            ),
        ])
    }
}

/// toplevel を掴んで置き直す。
///
/// 出来事は検証側が組み立てる。**握ったのは人ではない** ので、呼ぶ側は必ず
/// そう申告すること。掴みの算術も、台帳への書き戻しも、製品のバイナリが通るのと
/// 同じ道 ([`SchorlSession::apply`]) を通る。
fn measure_grab(session: &mut SchorlSession, window: &WindowId) -> Result<GrabMeasurement> {
    let placement = session
        .placements()
        .into_iter()
        .find(|p| &p.id == window)
        .ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "the window vanished before the grab could be measured",
                TraceId::unattributed(),
            )
        })?;
    let before = placement.plane.pose().pose.position;
    // 面の上を指す一点。面の中心から法線方向へ少し離れた場所に手を置く。
    let hand = before.add(placement.plane.normal_axis().scale(0.5));
    let mut outcome = StepOutcome::default();
    let mut effects = Vec::new();

    // ここから下は step() を挟まない。挟むと本物のランタイムの姿勢が混ざり、
    // 何が動かしたのか分からなくなる。
    session.apply(&pose_event(hand), 0, &mut outcome)?;
    session.apply(
        &XrEvent::GrabButton {
            controller: ControllerId::Right,
            state: PressState::Pressed,
        },
        0,
        &mut outcome,
    )?;
    session.apply(
        &pose_event(hand.add(Vec3::new(GRAB_SHIFT_M, 0.0, 0.0))),
        0,
        &mut outcome,
    )?;
    session.apply(
        &XrEvent::GrabButton {
            controller: ControllerId::Right,
            state: PressState::Released,
        },
        0,
        &mut outcome,
    )?;
    for effect in &outcome.grabs {
        effects.push(match effect {
            GrabEffect::Grabbed { window, .. } => format!("grabbed {}", window.as_str()),
            GrabEffect::Moved { window, pose } => format!(
                "moved {} to ({:.3},{:.3},{:.3})",
                window.as_str(),
                pose.pose.position.x,
                pose.pose.position.y,
                pose.pose.position.z
            ),
            GrabEffect::Released { window } => format!("released {}", window.as_str()),
        });
    }

    let after = session
        .window_pose(window)
        .ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "the window has no pose after the grab",
                TraceId::unattributed(),
            )
        })?
        .pose
        .position;

    let wanted = before.add(Vec3::new(GRAB_SHIFT_M, 0.0, 0.0));
    let off = after.sub(wanted).length();
    if off > 1e-3 {
        return Err(Error::new(
            ErrorCode::Internal,
            "the toplevel did not follow the controller by the amount the controller moved",
            TraceId::unattributed(),
        )
        .with_detail("off_mm", (off * 1000.0) as i64));
    }

    // 離したあとも同じところに在ること (`ux.window_not_head_locked` の下流)。
    session.step(false)?;
    let still = session
        .window_pose(window)
        .map(|p| p.pose.position.sub(after).length() < 1e-3)
        .unwrap_or(false);

    Ok(GrabMeasurement {
        window: window.as_str().to_owned(),
        before,
        after,
        wanted_shift_m: GRAB_SHIFT_M,
        effects,
        still_there_after_release: still,
    })
}

fn pose_event(position: Vec3) -> XrEvent {
    XrEvent::ControllerPose {
        controller: ControllerId::Right,
        pose: Pose::new(position, Quat::IDENTITY),
    }
}

/// swapchain の view 0 を一枚読み戻す。
fn capture(session: &mut SchorlSession, attempts: usize) -> Result<Vec<u8>> {
    for _ in 0..attempts {
        let outcome = session.step(true)?;
        if let Some(image) = outcome.captured {
            return Ok(image);
        }
        if outcome.exiting {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(Error::new(
        ErrorCode::HostRefused,
        "no swapchain image came back, so nothing could be inspected",
        TraceId::unattributed(),
    ))
}

fn spawn_probe(program_name: &str, socket: &str) -> Result<Child> {
    let program = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(program_name)))
        .filter(|p| p.is_file())
        .ok_or_else(|| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "the probe client is not next to this binary",
                TraceId::unattributed(),
            )
            .with_detail("program", program_name)
        })?;
    Command::new(program)
        .env("WAYLAND_DISPLAY", socket)
        .env_remove("DISPLAY")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            Error::new(
                ErrorCode::HostRefused,
                "could not start the probe client",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })
}

/// クライアントが描く前に出した一行を読む。
///
/// `prefix` はそのクライアントが名乗る綴り。経路ごとに別のバイナリが在るので、
/// **どのクライアントの申告を読んだかを取り違えない**ために呼び手が渡す。
fn read_announcement(child: &mut Child, prefix: &str) -> Result<QuadPattern> {
    let stdout = child.stdout.take().ok_or_else(|| {
        Error::new(
            ErrorCode::Internal,
            "the probe client has no stdout to announce on",
            TraceId::unattributed(),
        )
    })?;
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).map_err(|e| {
        Error::new(
            ErrorCode::HostRefused,
            "could not read the probe client's announcement",
            TraceId::unattributed(),
        )
        .caused_by(e)
    })?;
    let letters = line.trim().strip_prefix(prefix).ok_or_else(|| {
        Error::new(
            ErrorCode::InvalidArgument,
            "the probe client did not announce a pattern",
            TraceId::unattributed(),
        )
        .with_detail("expected_prefix", prefix)
        .with_detail("line", line.trim())
    })?;
    QuadPattern::from_letters(letters).ok_or_else(|| {
        Error::new(
            ErrorCode::InvalidArgument,
            "the probe client announced something that is not four distinct colours",
            TraceId::unattributed(),
        )
        .with_detail("letters", letters)
    })
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn pattern_centroid(census: &ColourCensus, pattern: QuadPattern) -> Option<(f64, f64)> {
    let mut weight = 0.0f64;
    let mut x = 0.0f64;
    let mut y = 0.0f64;
    for colour in pattern.quadrants {
        let slot = colour as usize;
        let count = census.counts[slot] as f64;
        let centre = census.centroids[slot]?;
        x += centre.0 * count;
        y += centre.1 * count;
        weight += count;
    }
    (weight > 0.0).then(|| (x / weight, y / weight))
}

fn counts_json(census: &ColourCensus) -> JsonValue {
    JsonValue::Object(
        PureColour::ALL
            .into_iter()
            .map(|colour| {
                (
                    colour.letter().to_string(),
                    JsonValue::Int(census.counts[colour as usize] as i64),
                )
            })
            .collect(),
    )
}

fn spell(paths: &[String]) -> JsonValue {
    JsonValue::Array(paths.iter().map(|p| JsonValue::text(p.clone())).collect())
}

/// 読み戻した絵を PPM (P6) で書き出す。見て確かめるのは人の仕事である。
fn write_ppm(path: &str, bytes: &[u8], width: u32, height: u32) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "P6\n{width} {height}\n255\n")?;
    for chunk in bytes.chunks_exact(4) {
        out.write_all(&[chunk[2], chunk[1], chunk[0]])?;
    }
    out.flush()
}

/// json 一行のログ (`pin code.log.format` / `pin code.log.required_fields`)。
fn emit(sink: &dyn LogSink, clock: &dyn Clock, level: Level, message: &str) {
    let trace = Id::new(IdScheme::Ulid, "01JSCHORLSEAMCHECK0000000")
        .map(TraceId::new)
        .unwrap_or_else(|_| TraceId::unattributed());
    let record = LogRecord::new(clock.now_utc(), level, trace, message);
    let _ = sink.emit(&record);
}
