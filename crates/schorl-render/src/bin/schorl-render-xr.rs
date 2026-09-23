//! OpenXR のフルセッションを Vulkan で開き、黒い空間へ矩形のサーフェスを置いて
//! 実際に submit できるかを実測する。
//!
//! HMD は要らない。`monado-service` に `QWERTY_ENABLE=1` を与えると HMD 無しで
//! 頭とコントローラが出るので、そこへ向けて回す。
//!
//! 出るのは数字である。「submit できた」ではなく「何枚 submit したか」「そのうち
//! 何枚がサーフェスを含んでいたか」「背景を何回黒で clear したか」を
//! [`RenderFacts`](schorl_render::RenderFacts) から貼る。
//!
//! **これは HMD を被った受け入れではない** (`pin verify.no_green_substitute`:
//! `forbid schorl.machine_green = substitute_for_hmd_acceptance`)。ここで緑が出ても
//! 御主人様が Quest 3 を被って見た、という主張にはならない。

use schorl_core::error::Result;
use schorl_core::frame::{Frame, FrameOrigin, PixelFormat};
use schorl_core::id::{Id, IdScheme, TraceId};
use schorl_core::json::JsonValue;
use schorl_core::log::{Level, LogRecord, LogSink};
use schorl_core::time::{Clock, SystemClock};
use schorl_panel::math::{Pose, Quat, Vec3};
use schorl_render::dmabuf::import_own_export_for_measurement;
use schorl_render::facts::StdoutLogSink;
use schorl_render::space::{Surface, SurfaceSize};
use schorl_render::texture::Texture;
use schorl_render::xr::XrVulkanRuntime;
use schorl_xr::ThreadSleeper;

/// 貼る絵の寸法。VRAM の余地が実測で約 1.1GiB しかないので控えめに取る。
const TEXTURE_WIDTH: u32 = 256;
/// 縦。
const TEXTURE_HEIGHT: u32 = 144;
/// 出すフレーム数。
const FRAMES: usize = 90;
/// 板を置く高さ (メートル)。
///
/// 参照空間は STAGE なので原点は**床**である。y = 0 に置くと足元に出るので、
/// 目の高さあたりへ上げる。`free schorl.window.default_pose` の中での選択。
const EYE_HEIGHT_M: f32 = 1.5;

fn main() -> std::process::ExitCode {
    let sink = StdoutLogSink::new();
    let clock = SystemClock;
    match run(&sink, &clock) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            emit(
                &sink,
                &clock,
                Level::Error,
                &format!(
                    "xr submit probe failed: {}",
                    e.envelope().to_json().render()
                ),
            );
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(sink: &dyn LogSink, clock: &dyn Clock) -> Result<()> {
    let runtime = XrVulkanRuntime::new("schorl-render-xr")?;
    let mut session = runtime.open()?;
    emit(
        sink,
        clock,
        Level::Info,
        &format!("session opened: {}", facts_json(session.facts()).render()),
    );

    // 二枚置く。`pin v1.window_count = one_or_more` に上限は無いので、
    // 一枚しか出せない構成になっていないことをここで実際に踏む。
    let left = {
        let frame = pattern_frame(clock, 0)?;
        let texture = Texture::upload_frame(session.vulkan(), &frame)?;
        Surface::new(
            Pose::new(
                Vec3::new(-0.6, EYE_HEIGHT_M, -1.5),
                Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), 0.35),
            ),
            SurfaceSize::new(1.0, 0.56)?,
            texture,
        )
    };
    // 右の板は dmabuf 経路で貼る。ランタイムが作った device に dmabuf の拡張が
    // 入っていなければ退路 (shm) へ落ちる。**落ちたことは routes に出る**ので、
    // 「dmabuf で出した」と偽れない。
    let (right, producer) = if session.vulkan().supports_dmabuf_import() {
        let frame = pattern_frame(clock, 128)?;
        let measured = import_own_export_for_measurement(session.vulkan(), &frame)?;
        emit(
            sink,
            clock,
            Level::Info,
            &format!(
                "dmabuf imported inside the XR session: {:?}",
                measured.imported()
            ),
        );
        let (imported, producer) = measured.into_imported();
        let texture = Texture::from_dmabuf(session.vulkan(), imported)?;
        (
            Surface::new(
                Pose::new(
                    Vec3::new(0.6, EYE_HEIGHT_M, -1.5),
                    Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), -0.35),
                ),
                SurfaceSize::new(1.0, 0.56)?,
                texture,
            ),
            Some(producer),
        )
    } else {
        emit(
            sink,
            clock,
            Level::Warn,
            "the runtime's Vulkan device has no dma_buf path; the right surface falls back to shm",
        );
        let frame = pattern_frame(clock, 128)?;
        let texture = Texture::upload_frame(session.vulkan(), &frame)?;
        (
            Surface::new(
                Pose::new(
                    Vec3::new(0.6, EYE_HEIGHT_M, -1.5),
                    Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), -0.35),
                ),
                SurfaceSize::new(1.0, 0.56)?,
                texture,
            ),
            None,
        )
    };
    let surfaces = [left, right];
    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "surfaces placed: {}",
            JsonValue::Object(vec![
                ("count".into(), JsonValue::Int(surfaces.len() as i64)),
                (
                    "routes".into(),
                    JsonValue::Array(
                        surfaces
                            .iter()
                            .map(|s| JsonValue::text(s.texture.route().as_str()))
                            .collect()
                    )
                ),
            ])
            .render()
        ),
    );

    let sleeper = ThreadSleeper;
    if !session.wait_until_running(&sleeper)? {
        // 走り出さなかった。**「出した」と数えない。**
        emit(
            sink,
            clock,
            Level::Warn,
            &format!(
                "the session never reached RUNNING: {}",
                JsonValue::Array(
                    session
                        .facts()
                        .observed_states
                        .iter()
                        .map(|s| JsonValue::text(s.clone()))
                        .collect()
                )
                .render()
            ),
        );
        session.close()?;
        return Err(schorl_core::Error::new(
            schorl_core::ErrorCode::HostRefused,
            "the OpenXR session never reached the RUNNING state, so no frame was submitted",
            TraceId::unattributed(),
        ));
    }

    let mut exiting = false;
    let mut captured: Option<Vec<u8>> = None;
    for index in 0..FRAMES {
        if session.pump_events()? {
            exiting = true;
            break;
        }
        if !session.is_running() {
            break;
        }
        // 最後の一枚だけ、ランタイムへ渡す絵をそのまま覗く。毎フレーム覗くと
        // 読み戻しが描画を待たせるので、一枚に絞る。
        let want_capture = index + 1 == FRAMES && captured.is_none();
        let (_, image) = session.submit_frame_capturing(&surfaces, want_capture)?;
        if let Some(image) = image {
            captured = Some(image);
        }
    }

    // 渡した絵の中身を検める。**「xrEndFrame を呼んだ」だけでは、自分の黒と板が
    // その絵に入っていた証拠にならない。**
    let extent = session.extent();
    if let Some(image) = captured.as_ref() {
        let report = inspect_submitted_image(image, extent.width, extent.height);
        emit(
            sink,
            clock,
            Level::Info,
            &format!(
                "inspected the image handed to the runtime: {}",
                report.to_json().render()
            ),
        );
        if report.corner_is_black_count != 4 {
            return Err(schorl_core::Error::new(
                schorl_core::ErrorCode::Internal,
                "the corners of the submitted image are not black, so the background is not owned by schorl",
                TraceId::unattributed(),
            )
            .with_detail("corner_is_black_count", report.corner_is_black_count as i64));
        }
        if report.non_black_pixels == 0 {
            return Err(schorl_core::Error::new(
                schorl_core::ErrorCode::Internal,
                "the submitted image is entirely black, so no surface was drawn into it",
                TraceId::unattributed(),
            ));
        }
        if report.distinct_colours < 3 {
            return Err(schorl_core::Error::new(
                schorl_core::ErrorCode::Internal,
                "the submitted image has too few distinct colours to be the rendered scene",
                TraceId::unattributed(),
            )
            .with_detail("distinct_colours", report.distinct_colours as i64));
        }
        // 人が目で見られる形でも残す。**これは受け入れではない。**
        // 実機を被っての確認は御主人様の身体が要る (`pin verify.hmd_gate`)。
        if let Ok(path) = std::env::var("SCHORL_RENDER_DUMP") {
            match write_ppm(&path, image, extent.width, extent.height) {
                Ok(()) => emit(
                    sink,
                    clock,
                    Level::Info,
                    &format!("wrote the submitted image to {path} (not an acceptance)"),
                ),
                Err(e) => emit(
                    sink,
                    clock,
                    Level::Warn,
                    &format!("could not write the submitted image: {e}"),
                ),
            }
        }
    } else {
        emit(
            sink,
            clock,
            Level::Warn,
            "no swapchain image was captured, so the contents of the submitted frame were not inspected",
        );
    }

    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "render ledger: {}",
            session.render_facts().to_json().render()
        ),
    );
    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "session states observed: {}",
            JsonValue::Array(
                session
                    .facts()
                    .observed_states
                    .iter()
                    .map(|s| JsonValue::text(s.clone()))
                    .collect()
            )
            .render()
        ),
    );
    let submitted = session.render_facts().frames_submitted;
    let with_surface = session.render_facts().frames_with_surface;
    let dmabuf_draws = session.render_facts().dmabuf_draws;
    let shm_draws = session.render_facts().shm_draws;

    // 受け側 (テクスチャ) を先に落として、そのあと提出側を返す
    // (`house.resource_lifecycle.same_scope` / `explicit_escape`)。
    session.vulkan().wait_idle()?;
    drop(surfaces);
    if let Some(producer) = producer {
        // SAFETY: 直前の wait_idle で device は空で、受け側は上の drop で落ちている。
        unsafe { producer.release(session.vulkan()) };
    }
    session.close()?;

    if submitted == 0 {
        return Err(schorl_core::Error::new(
            schorl_core::ErrorCode::HostRefused,
            "no frame reached xrEndFrame",
            TraceId::unattributed(),
        )
        .with_detail("exiting", exiting));
    }
    if with_surface == 0 {
        return Err(schorl_core::Error::new(
            schorl_core::ErrorCode::Internal,
            "frames were submitted but none carried a surface",
            TraceId::unattributed(),
        )
        .with_detail("frames_submitted", submitted as i64));
    }
    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "frames reached the runtime's swapchain; this is not an HMD acceptance: {}",
            JsonValue::Object(vec![
                ("dmabuf_draws".into(), JsonValue::Int(dmabuf_draws as i64)),
                ("shm_draws".into(), JsonValue::Int(shm_draws as i64)),
            ])
            .render()
        ),
    );
    Ok(())
}

/// 覗いた絵を PPM (P6) として書き出す。
///
/// 画素は `VK_FORMAT_B8G8R8A8_*` なので byte 0 が B、2 が R である。PPM は RGB 順
/// なので入れ替えて書く。**見て確かめるのは人の仕事で、この関数は受け入れを
/// 主張しない。**
fn write_ppm(path: &str, bytes: &[u8], width: u32, height: u32) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "P6\n{width} {height}\n255\n")?;
    for chunk in bytes.chunks_exact(4) {
        out.write_all(&[chunk[2], chunk[1], chunk[0]])?;
    }
    out.flush()
}

/// 渡した絵を覗いて分かったこと。
#[derive(Debug)]
struct SubmittedImageReport {
    /// 四隅のうち黒だったものの数。背景を自分で持っているなら 4。
    corner_is_black_count: usize,
    /// 黒でない画素の数。板が描かれていれば 0 より大きい。
    non_black_pixels: usize,
    /// 総画素数。
    total_pixels: usize,
    /// 出てきた色の数。一色なら読み戻しが何も証明していない。
    distinct_colours: usize,
}

impl SubmittedImageReport {
    fn to_json(&self) -> JsonValue {
        JsonValue::Object(vec![
            (
                "corner_is_black_count".into(),
                JsonValue::Int(self.corner_is_black_count as i64),
            ),
            (
                "non_black_pixels".into(),
                JsonValue::Int(self.non_black_pixels as i64),
            ),
            (
                "total_pixels".into(),
                JsonValue::Int(self.total_pixels as i64),
            ),
            (
                "distinct_colours".into(),
                JsonValue::Int(self.distinct_colours as i64),
            ),
        ])
    }
}

/// ランタイムへ渡した絵を覗く。
///
/// 見るのは三つ。(1) 四隅が黒であること — 背景を自分で書いているなら板の外は黒。
/// (2) 黒でない画素が在ること — 板が実際に描かれている。(3) 色が二色より多いこと —
/// 読み戻しが一色に潰れていないことの較正。
fn inspect_submitted_image(bytes: &[u8], width: u32, height: u32) -> SubmittedImageReport {
    let stride = width as usize * 4;
    let at = |x: usize, y: usize| -> [u8; 3] {
        let base = y * stride + x * 4;
        if base + 3 <= bytes.len() {
            [bytes[base], bytes[base + 1], bytes[base + 2]]
        } else {
            [0, 0, 0]
        }
    };
    let w = width as usize;
    let h = height as usize;
    let corners = [
        at(0, 0),
        at(w.saturating_sub(1), 0),
        at(0, h.saturating_sub(1)),
        at(w.saturating_sub(1), h.saturating_sub(1)),
    ];
    let corner_is_black_count = corners.iter().filter(|c| **c == [0, 0, 0]).count();
    let mut non_black_pixels = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for chunk in bytes.chunks_exact(4) {
        let rgb = [chunk[0], chunk[1], chunk[2]];
        if rgb != [0, 0, 0] {
            non_black_pixels += 1;
        }
        seen.insert(rgb);
    }
    SubmittedImageReport {
        corner_is_black_count,
        non_black_pixels,
        total_pixels: w * h,
        distinct_colours: seen.len(),
    }
}

/// 見分けのつく模様を一枚。
///
/// `FrameOrigin::TestDouble` である。**実 capture ではない**ので
/// `Frame::is_real_capture` が偽であることを型が保つ。
fn pattern_frame(clock: &dyn Clock, phase: u32) -> Result<Frame> {
    let stride = TEXTURE_WIDTH * PixelFormat::Xrgb8888.bytes_per_pixel();
    let mut pixels = Vec::with_capacity((stride * TEXTURE_HEIGHT) as usize);
    for y in 0..TEXTURE_HEIGHT {
        for x in 0..TEXTURE_WIDTH {
            // 16 画素の市松に、縁だけ明るい線を引く。姿勢と向きが目で追える形。
            let edge = x < 2 || y < 2 || x + 2 >= TEXTURE_WIDTH || y + 2 >= TEXTURE_HEIGHT;
            let checker = ((x / 16) + (y / 16)) % 2 == 0;
            let (b, g, r) = if edge {
                (255u8, 255, 255)
            } else if checker {
                (((x + phase) % 256) as u8, 40, 90)
            } else {
                (20, ((y + phase) % 256) as u8, 30)
            };
            pixels.push(b);
            pixels.push(g);
            pixels.push(r);
            pixels.push(0xff);
        }
    }
    Frame::new(
        FrameOrigin::TestDouble,
        PixelFormat::Xrgb8888,
        TEXTURE_WIDTH,
        TEXTURE_HEIGHT,
        stride,
        clock.now_utc(),
        pixels,
    )
}

/// セッションの事実を json へ。
fn facts_json(facts: &schorl_render::XrVulkanFacts) -> JsonValue {
    JsonValue::Object(vec![
        (
            "runtime_name".into(),
            JsonValue::text(facts.runtime_name.clone()),
        ),
        (
            "runtime_version".into(),
            JsonValue::text(facts.runtime_version.clone()),
        ),
        (
            "advertised_extension_count".into(),
            JsonValue::Int(facts.advertised_extension_count as i64),
        ),
        (
            "vulkan_enable2_advertised".into(),
            JsonValue::Bool(facts.vulkan_enable2_advertised),
        ),
        ("view_count".into(), JsonValue::Int(facts.view_count as i64)),
        (
            "offered_blend_modes".into(),
            JsonValue::Array(
                facts
                    .offered_blend_modes
                    .iter()
                    .map(|m| JsonValue::text(m.clone()))
                    .collect(),
            ),
        ),
        (
            "chosen_blend_mode".into(),
            JsonValue::text(facts.chosen_blend_mode.clone()),
        ),
        (
            "advertised_swapchain_format_count".into(),
            JsonValue::Int(facts.advertised_swapchain_format_count as i64),
        ),
        (
            "chosen_swapchain_format".into(),
            JsonValue::Int(i64::from(facts.chosen_swapchain_format)),
        ),
        (
            "reference_space".into(),
            JsonValue::text(facts.reference_space.clone()),
        ),
        (
            "vulkan_device".into(),
            JsonValue::text(facts.vulkan.device_name.clone()),
        ),
        (
            "vulkan_dmabuf_extensions_present".into(),
            JsonValue::Bool(facts.vulkan.dmabuf_extensions_present),
        ),
    ])
}

/// json 一行のログ (`pin code.log.format` / `pin code.log.required_fields`)。
fn emit(sink: &dyn LogSink, clock: &dyn Clock, level: Level, message: &str) {
    let trace = Id::new(IdScheme::Ulid, "01JSCHORLRENDERXRSUBMIT00")
        .map(TraceId::new)
        .unwrap_or_else(|_| TraceId::unattributed());
    let record = LogRecord::new(clock.now_utc(), level, trace, message);
    let _ = sink.emit(&record);
}
