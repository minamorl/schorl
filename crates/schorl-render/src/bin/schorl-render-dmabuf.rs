//! dmabuf を `VkImage` として import できるか実測する。
//!
//! 本物の Wayland クライアントは別 lane が持つので、ここでは提出側も自分で作る。
//! 同じ device で `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` の image を起こし、
//! 既知の模様を書き込み、`vkGetMemoryFdKHR` で dmabuf として取り出し、**別の
//! `VkImage` として import し直して**、sampler から読んだ結果を CPU へ戻して
//! 模様が一致するかを見る。
//!
//! 一致を見るのが要点である。import が `VK_SUCCESS` を返しただけでは、
//! 提出された画素がこちらへ届いた証拠にならない
//! (`pin verify.machine_scope` の `client_frame_reaches_swapchain` が
//! 「実フレームが届く」という不変条件を要求している)。
//!
//! shm の退路も同じ模様で通し、両方の経路が生きていることを一度に測る。
//!
//! **これは HMD を被った受け入れではない** (`pin verify.no_green_substitute`)。

use ash::vk;
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::frame::{Frame, FrameOrigin, PixelFormat};
use schorl_core::id::{Id, IdScheme, TraceId};
use schorl_core::json::JsonValue;
use schorl_core::log::{Level, LogRecord, LogSink};
use schorl_core::time::{Clock, SystemClock};
use schorl_render::dmabuf::{
    DmabufDescriptor, DmabufImage, DrmFormat, export_dmabuf, query_modifiers,
};
use schorl_render::facts::{StdoutLogSink, TextureRoute};
use schorl_render::texture::Texture;
use schorl_render::vulkan::VulkanContext;

/// 測る絵の寸法。VRAM の余地が実測で約 1.1GiB しかないので小さく取る。
const WIDTH: u32 = 64;
/// 縦。
const HEIGHT: u32 = 48;

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
                &format!("dmabuf probe failed: {}", e.envelope().to_json().render()),
            );
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(sink: &dyn LogSink, clock: &dyn Clock) -> Result<()> {
    let context = VulkanContext::standalone("schorl-render-dmabuf")?;
    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "vulkan device ready: {}",
            device_facts_json(&context).render()
        ),
    );

    if !context.supports_dmabuf_import() {
        // 退路だけを測って、import が組めなかったことを封筒ではなく事実として出す。
        // **「dmabuf が通った」と偽らない。**
        emit(
            sink,
            clock,
            Level::Warn,
            "this device has no dma_buf path; measuring the shm fallback only",
        );
        let pattern = pattern_frame(clock)?;
        let texture = Texture::upload_frame(&context, &pattern)?;
        assert_route(&texture, TextureRoute::Shm)?;
        emit(sink, clock, Level::Info, "shm fallback loaded a texture");
        return Ok(());
    }

    let format = DrmFormat::XRGB8888;
    let vk_format = format.to_vk_format().ok_or_else(|| {
        Error::new(
            ErrorCode::Internal,
            "the pinned fourcc lost its Vulkan format",
            TraceId::unattributed(),
        )
    })?;
    let modifiers = query_modifiers(
        &context,
        vk_format,
        vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::TRANSFER_DST,
    )?;
    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "drm format modifiers for {}: {}",
            format.as_fourcc_string(),
            JsonValue::Array(
                modifiers
                    .iter()
                    .map(|(m, planes)| JsonValue::Object(vec![
                        ("modifier".into(), JsonValue::Int(m.as_u64() as i64)),
                        ("vendor".into(), JsonValue::Int(i64::from(m.vendor()))),
                        ("plane_count".into(), JsonValue::Int(i64::from(*planes))),
                    ]))
                    .collect()
            )
            .render()
        ),
    );
    if modifiers.is_empty() {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "the device advertises no sampleable DRM format modifier for this fourcc",
            TraceId::unattributed(),
        ));
    }
    // 面が一つの modifier だけを使う。複数面の配置は v1 の import が扱わない。
    let single_plane: Vec<u64> = modifiers
        .iter()
        .filter(|(_, planes)| *planes == 1)
        .map(|(m, _)| m.as_u64())
        .collect();
    if single_plane.is_empty() {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "no single-plane DRM format modifier is available for this fourcc",
            TraceId::unattributed(),
        ));
    }

    let pattern = pattern_frame(clock)?;

    // 1. 提出側を作る。クライアントが持っているはずの dmabuf にあたる。
    let producer = Producer::create(&context, vk_format, &single_plane)?;
    producer.fill(&context, &pattern)?;
    // SAFETY: producer の image は DRM tiling で作り、memory を dedicated に
    // 束ねたもの。export に必要な条件を満たしている。
    let exported = unsafe {
        export_dmabuf(
            &context,
            producer.image,
            producer.memory,
            WIDTH,
            HEIGHT,
            format,
        )
    }?;
    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "exported a dma_buf: {}",
            JsonValue::Object(vec![
                (
                    "modifier".into(),
                    JsonValue::Int(exported.chosen_modifier.as_u64() as i64)
                ),
                (
                    "vendor".into(),
                    JsonValue::Int(i64::from(exported.chosen_modifier.vendor()))
                ),
                (
                    "plane_count".into(),
                    JsonValue::Int(exported.descriptor.planes.len() as i64)
                ),
                (
                    "stride".into(),
                    JsonValue::Int(exported.descriptor.planes[0].stride() as i64)
                ),
                (
                    "offset".into(),
                    JsonValue::Int(exported.descriptor.planes[0].offset() as i64)
                ),
                (
                    "fd".into(),
                    JsonValue::Int(i64::from(exported.descriptor.planes[0].raw_fd()))
                ),
            ])
            .render()
        ),
    );

    // 2. 受け側として import し直す。ここが compositor がやることと同じ形である。
    let descriptor = DmabufDescriptor {
        width_px: exported.descriptor.width_px,
        height_px: exported.descriptor.height_px,
        format: exported.descriptor.format,
        modifier: exported.descriptor.modifier,
        planes: exported.descriptor.planes,
    };
    let imported = DmabufImage::import(&context, descriptor)?;
    emit(
        sink,
        clock,
        Level::Info,
        &format!("imported the dma_buf as a VkImage: {imported:?}"),
    );
    let texture = Texture::from_dmabuf(&context, imported)?;
    assert_route(&texture, TextureRoute::Dmabuf)?;

    // 3. 読み戻して模様を照合する。import が成功しただけでは足りない。
    let read_back = sample_to_host(&context, &texture, vk_format)?;
    let expected = pattern.pixels();
    let mismatches = count_mismatches(expected, &read_back);
    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "sampled the imported image back to host memory: {}",
            JsonValue::Object(vec![
                ("bytes".into(), JsonValue::Int(read_back.len() as i64)),
                (
                    "mismatching_pixels".into(),
                    JsonValue::Int(mismatches as i64)
                ),
                (
                    "first_expected".into(),
                    JsonValue::Int(u32::from_le_bytes([
                        expected[0],
                        expected[1],
                        expected[2],
                        expected[3]
                    ]) as i64)
                ),
                (
                    "first_actual".into(),
                    JsonValue::Int(u32::from_le_bytes([
                        read_back[0],
                        read_back[1],
                        read_back[2],
                        read_back[3]
                    ]) as i64)
                ),
            ])
            .render()
        ),
    );
    if mismatches != 0 {
        return Err(Error::new(
            ErrorCode::Internal,
            "the pixels read back through the imported dma_buf do not match what the producer wrote",
            TraceId::unattributed(),
        )
        .with_detail("mismatching_pixels", mismatches as i64));
    }

    // 比べる道具そのものを較正する。答えの分かっている入力を先に通す。
    // 一致が 0 件だったことに意味を持たせるには、(a) 読んだ絵が一色でないこと、
    // (b) 食い違う入力なら食い違いを数えることの二つが要る。
    // **どちらかが欠けていれば「0 件一致」は検出できていないことと区別できない。**
    let distinct = distinct_pixels(&read_back);
    let shifted = shift_one_channel(expected);
    let control = count_mismatches(&shifted, &read_back);
    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "comparator calibration: {}",
            JsonValue::Object(vec![
                (
                    "distinct_pixels_read_back".into(),
                    JsonValue::Int(distinct as i64)
                ),
                (
                    "total_pixels".into(),
                    JsonValue::Int((WIDTH * HEIGHT) as i64)
                ),
                (
                    "mismatches_against_shifted".into(),
                    JsonValue::Int(control as i64)
                ),
            ])
            .render()
        ),
    );
    if distinct < 2 {
        return Err(Error::new(
            ErrorCode::Internal,
            "the image read back is a single flat colour, so a zero mismatch count proves nothing",
            TraceId::unattributed(),
        )
        .with_detail("distinct_pixels_read_back", distinct as i64));
    }
    if control == 0 {
        return Err(Error::new(
            ErrorCode::Internal,
            "the comparator reports no mismatch against deliberately shifted pixels, so it cannot detect a mismatch at all",
            TraceId::unattributed(),
        ));
    }

    // 4. 退路も同じ模様で通す。片方だけ生きている構成を緑にしない。
    let fallback = Texture::upload_frame(&context, &pattern)?;
    assert_route(&fallback, TextureRoute::Shm)?;
    let fallback_read = sample_to_host(&context, &fallback, vk_format)?;
    let fallback_mismatches = count_mismatches(expected, &fallback_read);
    emit(
        sink,
        clock,
        Level::Info,
        &format!(
            "shm fallback round trip: {}",
            JsonValue::Object(vec![(
                "mismatching_pixels".into(),
                JsonValue::Int(fallback_mismatches as i64)
            )])
            .render()
        ),
    );
    if fallback_mismatches != 0 {
        return Err(Error::new(
            ErrorCode::Internal,
            "the pixels read back through the shm fallback do not match the source",
            TraceId::unattributed(),
        )
        .with_detail("mismatching_pixels", fallback_mismatches as i64));
    }

    drop(texture);
    drop(fallback);
    producer.destroy(&context);
    context.wait_idle()?;
    emit(
        sink,
        clock,
        Level::Info,
        "both buffer routes carried the producer's pixels; this is not an HMD acceptance",
    );
    Ok(())
}

/// クライアント側にあたる image。export して渡す元。
struct Producer {
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl Producer {
    /// DRM tiling + export 可能な memory で作る。
    fn create(context: &VulkanContext, format: vk::Format, modifiers: &[u64]) -> Result<Self> {
        let device = context.device();
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // ash はこの二欄に setter を持たないので、生の欄へ直に書く。
        let mut list = vk::ImageDrmFormatModifierListCreateInfoEXT {
            drm_format_modifier_count: modifiers.len() as u32,
            p_drm_format_modifiers: modifiers.as_ptr(),
            ..Default::default()
        };
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: WIDTH,
                height: HEIGHT,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut list);
        // SAFETY: info の pNext 鎖と modifiers はこの場の生きた借用。
        let image = unsafe { device.create_image(&info, None) }.map_err(|e| {
            Error::new(
                ErrorCode::HostRefused,
                "vkCreateImage for the producer side failed",
                TraceId::unattributed(),
            )
            .with_detail("vk_result", e.as_raw() as i64)
        })?;
        // SAFETY: image は直前に作ったもの。
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let memory_type_index = context.find_memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut export)
            .push_next(&mut dedicated);
        // SAFETY: allocate の pNext 鎖はこの場の借用。
        let memory = unsafe { device.allocate_memory(&allocate, None) }.map_err(|e| {
            // SAFETY: image はこの関数が作ったもので、memory は束ねていない。
            unsafe { device.destroy_image(image, None) };
            Error::new(
                ErrorCode::HostRefused,
                "vkAllocateMemory for the producer side failed",
                TraceId::unattributed(),
            )
            .with_detail("vk_result", e.as_raw() as i64)
        })?;
        // SAFETY: image と memory はこの関数の組。
        unsafe { device.bind_image_memory(image, memory, 0) }.map_err(|e| {
            // SAFETY: どちらもこの関数が作ったもの。
            unsafe {
                device.free_memory(memory, None);
                device.destroy_image(image, None);
            }
            Error::new(
                ErrorCode::HostRefused,
                "vkBindImageMemory for the producer side failed",
                TraceId::unattributed(),
            )
            .with_detail("vk_result", e.as_raw() as i64)
        })?;
        Ok(Self { image, memory })
    }

    /// 既知の模様を書き込む。
    fn fill(&self, context: &VulkanContext, frame: &Frame) -> Result<()> {
        schorl_render::texture::stage_pixels_into_image(
            context,
            self.image,
            frame,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        )
    }

    /// 返す。
    fn destroy(self, context: &VulkanContext) {
        // SAFETY: この型が作った image と memory を一度だけ壊す。
        unsafe {
            let device = context.device();
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// 既知の模様を一枚作る。
///
/// `FrameOrigin::TestDouble` を使う。**これは実 capture ではない**ので、
/// `Frame::is_real_capture` が偽であることを型が保つ。
fn pattern_frame(clock: &dyn Clock) -> Result<Frame> {
    let stride = WIDTH * PixelFormat::Xrgb8888.bytes_per_pixel();
    let mut pixels = Vec::with_capacity((stride * HEIGHT) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            // B, G, R, X の順 (little endian の 0xXXRRGGBB)。
            pixels.push((x * 4 % 256) as u8);
            pixels.push((y * 5 % 256) as u8);
            pixels.push(((x + y) * 3 % 256) as u8);
            pixels.push(0xff);
        }
    }
    Frame::new(
        FrameOrigin::TestDouble,
        PixelFormat::Xrgb8888,
        WIDTH,
        HEIGHT,
        stride,
        clock.now_utc(),
        pixels,
    )
}

/// テクスチャを sampler から読んで CPU へ戻す。
///
/// import が成功しただけでは「画素が届いた」ことにならないので、実際に
/// fragment 段から読んだ結果を照合できる形にする。
fn sample_to_host(
    context: &VulkanContext,
    texture: &Texture,
    format: vk::Format,
) -> Result<Vec<u8>> {
    schorl_render::texture::sample_texture_to_host(context, texture, format, WIDTH, HEIGHT)
}

/// 経路が思っていた方であることを検める。
fn assert_route(texture: &Texture, expected: TextureRoute) -> Result<()> {
    if texture.route() == expected {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::Internal,
            "the texture took a different buffer route than the probe asked for",
            TraceId::unattributed(),
        )
        .with_detail("expected", expected.as_str())
        .with_detail("actual", texture.route().as_str()))
    }
}

/// 読み戻した絵に何色あるか。
///
/// 一色なら「一致 0 件」は何も証明しない。較正のための数。
fn distinct_pixels(bytes: &[u8]) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for chunk in bytes.chunks_exact(4) {
        seen.insert([chunk[0], chunk[1], chunk[2]]);
    }
    seen.len()
}

/// 一面だけずらした偽の期待値を作る。較正の対照。
fn shift_one_channel(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for chunk in out.chunks_exact_mut(4) {
        chunk[0] = chunk[0].wrapping_add(17);
    }
    out
}

/// 画素の食い違いを数える。
fn count_mismatches(expected: &[u8], actual: &[u8]) -> usize {
    if expected.len() != actual.len() {
        return expected.len().max(actual.len());
    }
    expected
        .chunks_exact(4)
        .zip(actual.chunks_exact(4))
        .filter(|(a, b)| {
            // X (alpha) の面は format が無視するので比べない。
            a[0] != b[0] || a[1] != b[1] || a[2] != b[2]
        })
        .count()
}

/// Vulkan の事実を json へ。
fn device_facts_json(context: &VulkanContext) -> JsonValue {
    let facts = context.facts();
    JsonValue::Object(vec![
        (
            "device_name".into(),
            JsonValue::text(facts.device_name.clone()),
        ),
        (
            "api_version".into(),
            JsonValue::text(facts.api_version.clone()),
        ),
        (
            "driver_version".into(),
            JsonValue::Int(i64::from(facts.driver_version)),
        ),
        (
            "queue_family_index".into(),
            JsonValue::Int(i64::from(facts.queue_family_index)),
        ),
        (
            "advertised_device_extension_count".into(),
            JsonValue::Int(facts.advertised_device_extension_count as i64),
        ),
        (
            "dmabuf_extensions_present".into(),
            JsonValue::Bool(facts.dmabuf_extensions_present),
        ),
        (
            "enabled_device_extensions".into(),
            JsonValue::Array(
                facts
                    .enabled_device_extensions
                    .iter()
                    .map(|n| JsonValue::text(n.clone()))
                    .collect(),
            ),
        ),
    ])
}

/// json 一行のログ (`pin code.log.format` / `pin code.log.required_fields`)。
fn emit(sink: &dyn LogSink, clock: &dyn Clock, level: Level, message: &str) {
    let trace = Id::new(IdScheme::Ulid, "01JSCHORLRENDERDMABUF0000")
        .map(TraceId::new)
        .unwrap_or_else(|_| TraceId::unattributed());
    let record = LogRecord::new(clock.now_utc(), level, trace, message);
    // ログの行き先が壊れても本題を止めない。
    let _ = sink.emit(&record);
}
