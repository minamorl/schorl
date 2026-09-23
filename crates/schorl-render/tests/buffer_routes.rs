//! 二つの buffer 経路が本当に画素を運ぶかを、実物の Vulkan で測る試験。
//!
//! この機には Vulkan が在るが、他のホストでは無いことがある。**前提が欠けた
//! ときに緑を返さない。** 何も測れなかった走りを `ok` として数えると、
//! `cargo test` の緑が「dmabuf 経路を測った」という意味を失う。だから前提の門は
//! 黙って `return` せず、何が無くて走れないかを名指しして落ちる。
//! (以前はここで `skipping: ...` と書いて `return` していた。Vulkan を潰した
//! 走りでも `6 passed; 0 failed` になってしまうので、その形を捨てた。)
//!
//! `pin verify.machine_scope` の `client_frame_reaches_swapchain` に対して
//! この試験が測るのは「提出された画素が受け側へ届く」ところまでである。
//! **HMD を被った受け入れではない** (`pin verify.no_green_substitute`)。
//!
//! # なぜ device を一つに畳んで直列にするか
//!
//! この機の空き VRAM は実測で 1GiB 未満である (`open_question schorl.vram_headroom`
//! が未測定としていた量で、いま `nvidia-smi` で 8188MiB 中 7345MiB 使用と出る)。
//! 試験ごとに `VkDevice` を起こすと、並列に走ったときドライバの中で掴み合って
//! 返らなくなる (実測: 三本同時で 130 秒経っても `rt_mutex_schedule` のまま)。
//! よって device は一つだけ作り、mutex で直列に使う。**並列で速くするより、
//! 返ってくることを採る。**

use schorl_core::error::ErrorCode;
use schorl_core::frame::{Frame, FrameOrigin, PixelFormat};
use schorl_core::testing::FixedClock;
use schorl_core::time::Clock;
use schorl_render::dmabuf::{
    DmabufImage, DrmFormat, DrmModifier, ExportableImage, import_own_export_for_measurement,
};
use schorl_render::facts::TextureRoute;
use schorl_render::texture::{Texture, sample_texture_to_host};
use schorl_render::vulkan::VulkanContext;

/// 小さい模様。VRAM の余地が実測で約 1.1GiB しかないので控えめに取る。
const WIDTH: u32 = 32;
/// 縦。
const HEIGHT: u32 = 24;

/// 段差のある模様を作る。一色だと「一致した」が何も証明しない。
fn pattern() -> Frame {
    let stride = WIDTH * PixelFormat::Xrgb8888.bytes_per_pixel();
    let mut pixels = Vec::with_capacity((stride * HEIGHT) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            pixels.push((x * 7 % 256) as u8);
            pixels.push((y * 11 % 256) as u8);
            pixels.push(((x * y) % 256) as u8);
            pixels.push(0xff);
        }
    }
    Frame::new(
        FrameOrigin::TestDouble,
        PixelFormat::Xrgb8888,
        WIDTH,
        HEIGHT,
        stride,
        FixedClock::at_millis(0).now_utc(),
        pixels,
    )
    .expect("the pattern dimensions agree")
}

/// 色の面 (B, G, R) だけを比べる。X の面は format が無視する。
fn mismatches(expected: &[u8], actual: &[u8]) -> usize {
    if expected.len() != actual.len() {
        return expected.len().max(actual.len());
    }
    expected
        .chunks_exact(4)
        .zip(actual.chunks_exact(4))
        .filter(|(a, b)| a[0] != b[0] || a[1] != b[1] || a[2] != b[2])
        .count()
}

/// 何色あるか。較正のための数。
fn distinct(bytes: &[u8]) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for chunk in bytes.chunks_exact(4) {
        seen.insert([chunk[0], chunk[1], chunk[2]]);
    }
    seen.len()
}

/// この試験ファイル全体で一つだけ持つ Vulkan の面。
///
/// `absence` が `Some` なら、このホストに使える Vulkan が無い。そのときは
/// **緑を返さず**、何が無いかを名指しして落ちる。
struct SharedVulkan {
    /// 使える device。`absence` が `Some` のときだけ `None` になる。
    context: std::sync::Mutex<Option<VulkanContext>>,
    /// device を起こせなかった理由 (封筒の message)。起こせたなら `None`。
    absence: Option<String>,
    /// この device が dma_buf の取り込み経路を持つか。
    ///
    /// 起こした時点で控えるのは、**門を mutex の外で引く**ためである。
    /// 借用の中で落とすと共有 device を毒し、dmabuf と関係の無い試験まで
    /// 「前の試験が毒した」という別の理由で落ちて、何が無いのかが読めなくなる。
    dmabuf_import: bool,
}

/// 一度だけ device を起こす置き場。
static VULKAN: std::sync::OnceLock<SharedVulkan> = std::sync::OnceLock::new();

/// device を一度だけ起こして借りる。
fn shared_vulkan() -> &'static SharedVulkan {
    VULKAN.get_or_init(|| match VulkanContext::standalone("schorl-render-test") {
        Ok(context) => SharedVulkan {
            dmabuf_import: context.supports_dmabuf_import(),
            context: std::sync::Mutex::new(Some(context)),
            absence: None,
        },
        Err(e) => {
            // 起こせなかった理由が想定の二つでないなら、それ自体が異常である。
            assert!(
                matches!(
                    e.code(),
                    ErrorCode::CapabilityUnavailable | ErrorCode::HostRefused
                ),
                "an unexpected error code for a missing Vulkan: {:?}",
                e.code()
            );
            SharedVulkan {
                context: std::sync::Mutex::new(None),
                absence: Some(e.message().to_owned()),
                dmabuf_import: false,
            }
        }
    })
}

/// 共有された device を借りて、閉じた形で一仕事する。
///
/// 借用の間は mutex を握るので、試験は互いに直列になる。
fn with_vulkan<F: FnOnce(&VulkanContext)>(body: F) {
    borrow_device(false, body);
}

/// 同じく借りるが、dma_buf の取り込み経路も前提として要求する。
fn with_dmabuf_vulkan<F: FnOnce(&VulkanContext)>(body: F) {
    borrow_device(true, body);
}

/// **前提の門はここに在る。**
///
/// 揃っていなければ、何も測らずに `ok` を返すのではなく、何が無いかを名指し
/// して落ちる。門はどちらも mutex を取る前に置いてあるので、前提が欠けたときの
/// panic が共有 device を毒すことはない。
fn borrow_device<F: FnOnce(&VulkanContext)>(needs_dmabuf: bool, body: F) {
    let shared = shared_vulkan();
    assert!(
        shared.absence.is_none(),
        "no usable Vulkan on this host, so this check would measure nothing: {}. \
         it needs a Vulkan loader plus an ICD this user can open; run the check on a \
         machine that has one.",
        shared.absence.as_deref().unwrap_or_default()
    );
    assert!(
        !needs_dmabuf || shared.dmabuf_import,
        "this Vulkan device has no dma_buf import path, so the dmabuf route would measure \
         nothing. run the check on a device whose driver offers dma_buf import of DRM \
         format modifiers."
    );
    // poison していたら、前の試験が panic した後である。そこを緑にしない。
    let guard = shared
        .context
        .lock()
        .expect("a previous test poisoned the shared device");
    let context = guard
        .as_ref()
        .expect("the gate above already refused a host without a usable Vulkan");
    body(context);
    context.wait_idle().expect("the device goes idle");
}

#[test]
fn the_shm_fallback_carries_every_pixel() {
    with_vulkan(|context| {
        let frame = pattern();
        let texture = Texture::upload_frame(context, &frame).expect("the shm upload succeeds");
        assert_eq!(texture.route(), TextureRoute::Shm);
        let read_back = sample_texture_to_host(
            context,
            &texture,
            ash::vk::Format::B8G8R8A8_UNORM,
            WIDTH,
            HEIGHT,
        )
        .expect("the read back succeeds");

        // 較正: 読んだ絵が一色でないこと。一色なら一致は何も言っていない。
        assert!(
            distinct(&read_back) > 1,
            "the image read back is flat, so a zero mismatch count proves nothing"
        );
        assert_eq!(mismatches(frame.pixels(), &read_back), 0);

        // 較正: 食い違う入力なら食い違いを数えること。
        let mut shifted = frame.pixels().to_vec();
        for chunk in shifted.chunks_exact_mut(4) {
            chunk[1] = chunk[1].wrapping_add(31);
        }
        assert!(
            mismatches(&shifted, &read_back) > 0,
            "the comparator cannot tell a mismatch, so its verdict is worthless"
        );

        drop(texture);
    });
}

#[test]
fn the_dmabuf_route_carries_every_pixel() {
    with_dmabuf_vulkan(|context| {
        let frame = pattern();
        let measured = import_own_export_for_measurement(context, &frame)
            .expect("the producer side exports and the consumer side imports");
        // 提出側が選んだ配置を記録に残す。linear とは限らない。
        let modifier = measured.imported().modifier();
        eprintln!(
            "imported a dma_buf with modifier {} (vendor {})",
            modifier.as_u64(),
            modifier.vendor()
        );
        let (imported, producer) = measured.into_imported();
        let texture =
            Texture::from_dmabuf(context, imported).expect("the imported image is sampleable");
        assert_eq!(texture.route(), TextureRoute::Dmabuf);
        let read_back = sample_texture_to_host(
            context,
            &texture,
            ash::vk::Format::B8G8R8A8_UNORM,
            WIDTH,
            HEIGHT,
        )
        .expect("the read back succeeds");

        assert!(
            distinct(&read_back) > 1,
            "the image read back is flat, so a zero mismatch count proves nothing"
        );
        assert_eq!(
            mismatches(frame.pixels(), &read_back),
            0,
            "the pixels the producer wrote did not survive the dma_buf round trip"
        );

        drop(texture);
        context.wait_idle().expect("the device goes idle");
        // SAFETY: 受け側は上の drop で落ち、device は idle になっている。
        unsafe { producer.release(context) };
    });
}

#[test]
fn both_routes_reach_the_same_pixels() {
    // 退路が本物の代わりになっていること。**片方だけ生きている構成を緑にしない。**
    with_dmabuf_vulkan(|context| {
        let frame = pattern();
        let shm = Texture::upload_frame(context, &frame).expect("the shm upload succeeds");
        let shm_read = sample_texture_to_host(
            context,
            &shm,
            ash::vk::Format::B8G8R8A8_UNORM,
            WIDTH,
            HEIGHT,
        )
        .expect("the read back succeeds");
        drop(shm);

        let measured = import_own_export_for_measurement(context, &frame)
            .expect("the dma_buf round trip works");
        let (imported, producer) = measured.into_imported();
        let dmabuf =
            Texture::from_dmabuf(context, imported).expect("the imported image is sampleable");
        let dmabuf_read = sample_texture_to_host(
            context,
            &dmabuf,
            ash::vk::Format::B8G8R8A8_UNORM,
            WIDTH,
            HEIGHT,
        )
        .expect("the read back succeeds");
        drop(dmabuf);

        assert_eq!(
            mismatches(&shm_read, &dmabuf_read),
            0,
            "the two buffer routes disagree on the same source pixels"
        );
        context.wait_idle().expect("the device goes idle");
        // SAFETY: 受け側は上の drop で落ち、device は idle になっている。
        unsafe { producer.release(context) };
    });
}

#[test]
fn an_unmappable_fourcc_is_refused_not_guessed() {
    // 知らない fourcc を黙って何かへ丸めない。
    let xrgb = DrmFormat::XRGB8888;
    assert!(xrgb.to_vk_format().is_some());
    // 文字列だけを頼りに存在しない形式を作れないことを、綴りの安定で示す。
    assert_eq!(xrgb.as_fourcc_string(), "XR24");
}

#[test]
fn an_exportable_image_hands_its_pixels_over_a_dma_buf_fd() {
    // `ExportableImage` はクライアント側の面である。ここで測るのは
    // 「export した fd が、受け側 (`DmabufImage::import`) で同じ画素になる」
    // ところまで。**`zwp_linux_dmabuf_v1` を越える一本通しは
    // `schorl-seam-check` が別に測る。**
    with_dmabuf_vulkan(|context| {
        let frame = pattern();
        let exportable = ExportableImage::create(
            context,
            WIDTH,
            HEIGHT,
            DrmFormat::XRGB8888,
            // compositor が申告する配置に合わせる。
            &[DrmModifier::LINEAR],
        )
        .expect("a linear exportable image can be created on this device");
        exportable
            .fill(context, &frame)
            .expect("the producer writes its pixels");
        context.wait_idle().expect("the device goes idle");
        let exported = exportable.export(context).expect("the dma_buf comes out");
        assert_eq!(
            exported.chosen_modifier,
            DrmModifier::LINEAR,
            "the allowed set asked for linear, so no other layout may be chosen silently"
        );
        assert_eq!(exported.descriptor.planes.len(), 1);
        assert!(
            exported.descriptor.planes[0].stride() >= u64::from(WIDTH) * 4,
            "a row cannot be shorter than its pixels"
        );

        let imported =
            DmabufImage::import(context, exported.descriptor).expect("the consumer side imports");
        let texture =
            Texture::from_dmabuf(context, imported).expect("the imported image is sampleable");
        assert_eq!(texture.route(), TextureRoute::Dmabuf);
        let read_back = sample_texture_to_host(
            context,
            &texture,
            ash::vk::Format::B8G8R8A8_UNORM,
            WIDTH,
            HEIGHT,
        )
        .expect("the read back succeeds");

        assert!(
            distinct(&read_back) > 1,
            "the image read back is flat, so a zero mismatch count proves nothing"
        );
        assert_eq!(
            mismatches(frame.pixels(), &read_back),
            0,
            "the pixels the producer wrote did not survive the exported dma_buf"
        );

        drop(texture);
        context.wait_idle().expect("the device goes idle");
        drop(exportable);
    });
}

#[test]
fn an_exportable_image_refuses_a_layout_the_device_does_not_offer() {
    // 許した配置が一つも使えないとき、**黙って別の配置へ丸めない。**
    // 丸めてしまうと、compositor が申告していない配置の dmabuf を出すことになる。
    with_dmabuf_vulkan(|context| {
        // vendor 0xfe は `drm_fourcc.h` のどの vendor でもない。
        let nonsense = DrmModifier::from_u64(0xfe00_0000_0000_0001);
        let refused =
            ExportableImage::create(context, WIDTH, HEIGHT, DrmFormat::XRGB8888, &[nonsense]);
        match refused {
            Ok(_) => panic!("a layout this device never advertised was accepted"),
            Err(e) => assert_eq!(e.code(), ErrorCode::Unsupported),
        }
    });
}
