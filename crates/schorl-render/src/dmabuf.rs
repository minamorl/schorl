//! dmabuf を `VkImage` として import する経路と、その退路を測るための export。
//!
//! `free schorl.compositor.buffer_import_path` は pin されていない軸である。
//! ここで選んだのは `VK_EXT_external_memory_dma_buf` +
//! `VK_EXT_image_drm_format_modifier` の組で、一次資料がまさにこの場面を
//! 名指しで例示している。
//!
//! `VK_EXT_external_memory_dma_buf` の逐語:
//!
//! > A `dma_buf` is a type of file descriptor, defined by the Linux kernel, that
//! > allows sharing memory across kernel device drivers and across processes.
//! > This extension enables applications to import a `dma_buf` as VkDeviceMemory,
//! > to export VkDeviceMemory as a `dma_buf`, and to create VkBuffer objects that
//! > **can** be bound to that memory.
//!
//! `VK_EXT_image_drm_format_modifier` の逐語 (Wayland compositor がこの場面):
//!
//! > a Wayland compositor and Wayland client may ... negotiate a vendor-specific
//! > tiling format for a shared `wl_buffer`
//!
//! `VkImportMemoryFdInfoKHR` の逐語 (fd の所有が移ること):
//!
//! > Importing memory from a file descriptor transfers ownership of the file
//! > descriptor from the application to the Vulkan implementation. The application
//! > **must** not perform any operations on the file descriptor after a successful
//! > import.
//!
//! この逐語により [`DmabufImage::import`] は fd を**消費する**署名にしてある。
//! 成功したら呼び手はその fd を二度と触れない。失敗したときは fd の所有が
//! 呼び手へ戻るよう [`DmabufPlane::fd`] を返す封筒にしていない — 失敗経路では
//! この型が閉じる。
//!
//! fourcc と modifier の綴りは Linux の `include/uapi/drm/drm_fourcc.h` の逐語:
//!
//! > `#define fourcc_code(a, b, c, d) ((__u32)(a) | ((__u32)(b) << 8) | ((__u32)(c) << 16) | ((__u32)(d) << 24))`
//! > `#define DRM_FORMAT_XRGB8888 fourcc_code('X', 'R', '2', '4') /* [31:0] x:R:G:B 8:8:8:8 little endian */`
//! > `#define DRM_FORMAT_ARGB8888 fourcc_code('A', 'R', '2', '4') /* [31:0] A:R:G:B 8:8:8:8 little endian */`
//! > `#define fourcc_mod_code(vendor, val) ((((__u64)DRM_FORMAT_MOD_VENDOR_## vendor) << 56) | ((val) & 0x00ffffffffffffffULL))`
//! > `#define DRM_FORMAT_MOD_LINEAR fourcc_mod_code(NONE, 0)`

use ash::vk;
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::frame::PixelFormat;
use schorl_core::id::TraceId;

use crate::vulkan::{VulkanContext, vulkan_failure};

/// DRM の fourcc。
///
/// `wl_shm` と `zwp_linux_dmabuf_v1` がバッファの画素並びに使う語彙である。
/// 値は `drm_fourcc.h` の `fourcc_code` をそのまま計算したもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DrmFormat(u32);

impl DrmFormat {
    /// `DRM_FORMAT_XRGB8888` = `fourcc_code('X','R','2','4')`。
    pub const XRGB8888: DrmFormat = DrmFormat(fourcc(b'X', b'R', b'2', b'4'));
    /// `DRM_FORMAT_ARGB8888` = `fourcc_code('A','R','2','4')`。
    pub const ARGB8888: DrmFormat = DrmFormat(fourcc(b'A', b'R', b'2', b'4'));

    /// 生の 32 bit。
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// クライアントが提出した生の fourcc を包む。
    ///
    /// 検めない。`drm_fourcc.h` の語は二枝より広く、こちらが上限を発明しては
    /// ならないからである。使えるかどうかは [`Self::to_vk_format`] が言う。
    pub const fn from_u32(value: u32) -> Self {
        Self(value)
    }

    /// 四文字の綴り。報告に貼るため。
    pub fn as_fourcc_string(self) -> String {
        let bytes = self.0.to_le_bytes();
        bytes.iter().map(|b| *b as char).collect()
    }

    /// 対応する `VkFormat`。
    ///
    /// `DRM_FORMAT_XRGB8888` は逐語で「[31:0] x:R:G:B 8:8:8:8 little endian」であり、
    /// little endian の 32 bit 語で byte 0 が B、byte 1 が G、byte 2 が R、byte 3 が x
    /// になる。`VK_FORMAT_B8G8R8A8_UNORM` の byte 順が同じなのでこれを充てる。
    /// x の面は無視されるが、[`crate::Renderer`] の fragment 段が alpha を 1 に
    /// 固めるので不透明になる。
    pub const fn to_vk_format(self) -> Option<vk::Format> {
        if self.0 == DrmFormat::XRGB8888.0 || self.0 == DrmFormat::ARGB8888.0 {
            Some(vk::Format::B8G8R8A8_UNORM)
        } else {
            None
        }
    }

    /// `schorl-core` の画素並びから。
    ///
    /// 逆向き ([`DrmFormat`] → [`PixelFormat`]) は作っていない。dmabuf の
    /// fourcc は `PixelFormat` の二枝より広いので、写す先が無い値がある。
    pub const fn from_pixel_format(format: PixelFormat) -> Self {
        match format {
            PixelFormat::Xrgb8888 => DrmFormat::XRGB8888,
            PixelFormat::Argb8888 => DrmFormat::ARGB8888,
        }
    }
}

/// DRM の format modifier。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DrmModifier(u64);

impl DrmModifier {
    /// `DRM_FORMAT_MOD_LINEAR` = `fourcc_mod_code(NONE, 0)` = 0。
    pub const LINEAR: DrmModifier = DrmModifier(0);

    /// 生の 64 bit から。
    pub const fn from_u64(value: u64) -> Self {
        DrmModifier(value)
    }

    /// 生の 64 bit。
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// 上位 8 bit の vendor id。`fourcc_mod_get_vendor` の逐語どおり。
    pub const fn vendor(self) -> u8 {
        ((self.0 >> 56) & 0xff) as u8
    }
}

/// dmabuf の一面。
///
/// `zwp_linux_buffer_params_v1::add` が一面ごとに渡す四つ組と同じ形である。
#[derive(Debug)]
pub struct DmabufPlane {
    /// dmabuf の file descriptor。**この型が所有する。**
    fd: std::os::fd::OwnedFd,
    /// その面の先頭までの byte offset。
    offset: u64,
    /// 一行の byte 数。
    stride: u64,
}

impl DmabufPlane {
    /// 所有した fd と配置から作る。
    pub const fn new(fd: std::os::fd::OwnedFd, offset: u64, stride: u64) -> Self {
        Self { fd, offset, stride }
    }

    /// byte offset。
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// 行の byte 数。
    pub const fn stride(&self) -> u64 {
        self.stride
    }

    /// 生の fd 番号。報告に貼るためだけの読み取りで、所有は移らない。
    pub fn raw_fd(&self) -> i32 {
        use std::os::fd::AsRawFd as _;
        self.fd.as_raw_fd()
    }
}

/// import できる dmabuf の記述。
///
/// クライアントが `zwp_linux_dmabuf_v1` で提出してくるものと同じ情報である。
/// 面の数は 1 に限っていない (`drm_format_modifier_plane_count` は modifier が
/// 決める量なので、こちらが上限を発明してはならない)。
#[derive(Debug)]
pub struct DmabufDescriptor {
    /// 横 (画素)。
    pub width_px: u32,
    /// 縦 (画素)。
    pub height_px: u32,
    /// 画素並び。
    pub format: DrmFormat,
    /// 配置。
    pub modifier: DrmModifier,
    /// 面。1 個以上。
    pub planes: Vec<DmabufPlane>,
}

/// import した結果の `VkImage` と、それを支える memory。
///
/// `house.resource_lifecycle.same_scope` により、image と memory は同じ scope で
/// 取って同じ scope で返す。`Drop` で両方壊す。
pub struct DmabufImage {
    device: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    width_px: u32,
    height_px: u32,
    format: vk::Format,
    modifier: DrmModifier,
    plane_count: u32,
}

impl core::fmt::Debug for DmabufImage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmabufImage")
            .field("width_px", &self.width_px)
            .field("height_px", &self.height_px)
            .field("format", &self.format)
            .field("modifier", &self.modifier)
            .field("plane_count", &self.plane_count)
            .finish_non_exhaustive()
    }
}

impl DmabufImage {
    /// 提出された dmabuf を `VkImage` として import する。
    ///
    /// 手順は `VK_EXT_image_drm_format_modifier` の逐語どおり:
    /// `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` の image を
    /// `VkImageDrmFormatModifierExplicitCreateInfoEXT` で作り、
    /// `VkImportMemoryFdInfoKHR` で memory を import して束ねる。
    ///
    /// `pPlaneLayouts` の各要素について `size` は 0 でなければならない
    /// (逐語: 「For each element of `pPlaneLayouts`, `size` **must** be 0」)。
    /// `arrayPitch` は `arrayLayers` が 1 のとき 0、`depthPitch` は
    /// `extent.depth` が 1 のとき 0 でなければならない。
    ///
    /// `descriptor` は消費する。中の fd の所有が Vulkan 実装へ移るためで、
    /// 逐語は [`crate::dmabuf`] の冒頭に引いてある。
    pub fn import(context: &VulkanContext, descriptor: DmabufDescriptor) -> Result<Self> {
        let Some(external_memory_fd) = context.external_memory_fd() else {
            return Err(Error::new(
                ErrorCode::CapabilityUnavailable,
                "this Vulkan device has no dma_buf import path, so the shm fallback must be used",
                TraceId::unattributed(),
            ));
        };
        if descriptor.planes.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "a dma_buf descriptor must carry at least one memory plane",
                TraceId::unattributed(),
            ));
        }
        if descriptor.width_px == 0 || descriptor.height_px == 0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "a dma_buf descriptor must have a non-zero extent",
                TraceId::unattributed(),
            ));
        }
        let format = descriptor.format.to_vk_format().ok_or_else(|| {
            Error::new(
                ErrorCode::Unsupported,
                "this DRM fourcc has no Vulkan format in the v1 mapping",
                TraceId::unattributed(),
            )
            .with_detail("fourcc", descriptor.format.as_fourcc_string())
        })?;

        let plane_layouts: Vec<vk::SubresourceLayout> = descriptor
            .planes
            .iter()
            .map(|plane| vk::SubresourceLayout {
                offset: plane.offset,
                // 逐語により 0 でなければならない。
                size: 0,
                row_pitch: plane.stride,
                // arrayLayers = 1 なので 0。
                array_pitch: 0,
                // extent.depth = 1 なので 0。
                depth_pitch: 0,
            })
            .collect();

        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(descriptor.modifier.as_u64())
            .plane_layouts(&plane_layouts);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: descriptor.width_px,
                height: descriptor.height_px,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut explicit);

        let device = context.device().clone();
        // SAFETY: image_info の pNext 鎖はこの場の借用で、呼び出しの間だけ生きる。
        let image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| vulkan_failure("vkCreateImage for the imported dma_buf failed", e))?;

        let bind = |image: vk::Image| -> Result<vk::DeviceMemory> {
            // SAFETY: image は直前に作ったもの。
            let requirements = unsafe { device.get_image_memory_requirements(image) };
            let first = &descriptor.planes[0];
            let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
            // SAFETY: fd はまだ呼び手 (この型) が持っている。問い合わせは所有を移さない。
            unsafe {
                external_memory_fd.get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    first.raw_fd(),
                    &mut fd_properties,
                )
            }
            .map_err(|e| vulkan_failure("vkGetMemoryFdPropertiesKHR failed", e))?;
            let allowed = requirements.memory_type_bits & fd_properties.memory_type_bits;
            let memory_type_index =
                context.find_memory_type(allowed, vk::MemoryPropertyFlags::empty())?;

            // fd の所有を Vulkan へ渡す。逐語により import が成功したあとは
            // こちらが触ってはならないので、dup した複製を渡すのではなく本体を
            // 渡し、この時点で Rust 側の所有を放す。
            let raw_fd = {
                use std::os::fd::IntoRawFd as _;
                let plane = descriptor.planes.into_iter().next().expect("checked above");
                plane.fd.into_raw_fd()
            };
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(raw_fd);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let allocate = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type_index)
                .push_next(&mut import)
                .push_next(&mut dedicated);
            // SAFETY: allocate の pNext 鎖はこの場の借用。fd の所有はここで移る。
            let memory = unsafe { device.allocate_memory(&allocate, None) }.map_err(|e| {
                // import が失敗したときは所有が移っていないので、こちらで閉じる。
                // SAFETY: raw_fd はこの関数が所有を引き取った本物の fd で、
                // Vulkan が受け取っていないので二重 close にならない。
                unsafe {
                    let _ = libc_close(raw_fd);
                }
                vulkan_failure("vkAllocateMemory importing the dma_buf failed", e)
            })?;
            // SAFETY: image と memory はこの関数が作った組。
            unsafe { device.bind_image_memory(image, memory, 0) }.map_err(|e| {
                // SAFETY: memory はこの関数が作ったもの。
                unsafe { device.free_memory(memory, None) };
                vulkan_failure("vkBindImageMemory for the imported dma_buf failed", e)
            })?;
            Ok(memory)
        };

        let plane_count = plane_layouts.len() as u32;
        match bind(image) {
            Ok(memory) => Ok(Self {
                device,
                image,
                memory,
                width_px: descriptor.width_px,
                height_px: descriptor.height_px,
                format,
                modifier: descriptor.modifier,
                plane_count,
            }),
            Err(e) => {
                // SAFETY: image はこの関数が作ったもので、memory は束ねていない。
                unsafe { device.destroy_image(image, None) };
                Err(e)
            }
        }
    }

    /// import した `VkImage`。
    pub const fn image(&self) -> vk::Image {
        self.image
    }

    /// 横 (画素)。
    pub const fn width_px(&self) -> u32 {
        self.width_px
    }

    /// 縦 (画素)。
    pub const fn height_px(&self) -> u32 {
        self.height_px
    }

    /// `VkFormat`。
    pub const fn format(&self) -> vk::Format {
        self.format
    }

    /// import に使った modifier。
    pub const fn modifier(&self) -> DrmModifier {
        self.modifier
    }

    /// 面の数。
    pub const fn plane_count(&self) -> u32 {
        self.plane_count
    }
}

impl Drop for DmabufImage {
    fn drop(&mut self) {
        // SAFETY: この型が所有した image と memory を一度だけ壊す。dmabuf の fd は
        // import の時点で Vulkan 実装へ所有が移っているので、こちらは触らない。
        unsafe {
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// export して作った dmabuf。**測定のためだけに在る。**
///
/// 本物のクライアントは別 lane が持つ。この crate が import 経路を実測するには
/// 提出側の dmabuf が要るので、同じ device で `VkImage` を作って
/// `vkGetMemoryFdKHR` で dmabuf として取り出す。取り出した記述は
/// [`DmabufImage::import`] がそのまま食える形で返る。
#[derive(Debug)]
pub struct ExportedDmabuf {
    /// import 側へ渡す記述。
    pub descriptor: DmabufDescriptor,
    /// export に使った modifier (実際に選ばれた値)。
    pub chosen_modifier: DrmModifier,
}

/// その format で使える modifier を並べる。
///
/// `VkDrmFormatModifierPropertiesListEXT` を `vkGetPhysicalDeviceFormatProperties2`
/// の pNext へ繋いで二度呼ぶ (数を取る → 埋める)。
pub fn query_modifiers(
    context: &VulkanContext,
    format: vk::Format,
    required_features: vk::FormatFeatureFlags,
) -> Result<Vec<(DrmModifier, u32)>> {
    if context.drm_format_modifier().is_none() {
        return Err(Error::new(
            ErrorCode::CapabilityUnavailable,
            "this Vulkan device does not expose VK_EXT_image_drm_format_modifier",
            TraceId::unattributed(),
        ));
    }
    // 一度目: 数だけ取る。pNext 鎖の借用が list を掴んでいるので、書き戻された
    // 数を読むのは借用が切れたあと。ブロックで囲って順序を型で守る。
    let count = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        {
            let mut properties = vk::FormatProperties2::default().push_next(&mut list);
            // SAFETY: properties の pNext 鎖はこの場の借用で、呼び出しの間だけ生きる。
            unsafe {
                context.instance().get_physical_device_format_properties2(
                    context.physical_device(),
                    format,
                    &mut properties,
                );
            }
        }
        list.drm_format_modifier_count as usize
    };
    if count == 0 {
        return Ok(Vec::new());
    }

    // 二度目: 数ぶんの入れ物を渡して埋めさせる。
    let mut storage = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
    {
        // ash はこの二欄に setter を持たないので、生の欄へ直に書く。
        let mut list = vk::DrmFormatModifierPropertiesListEXT {
            drm_format_modifier_count: count as u32,
            p_drm_format_modifier_properties: storage.as_mut_ptr(),
            ..Default::default()
        };
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: storage は count 個ぶん確保してあり、list がそこを指している。
        unsafe {
            context.instance().get_physical_device_format_properties2(
                context.physical_device(),
                format,
                &mut properties,
            );
        }
    }
    Ok(storage
        .into_iter()
        .filter(|p| {
            p.drm_format_modifier_tiling_features
                .contains(required_features)
        })
        .map(|p| {
            (
                DrmModifier::from_u64(p.drm_format_modifier),
                p.drm_format_modifier_plane_count,
            )
        })
        .collect())
}

/// `VkImage` を dmabuf として export し、import 側が食える記述を返す。
///
/// `image` と `memory` の所有は呼び手に残る。返る fd は `vkGetMemoryFdKHR` が
/// 作った新しい handle であり、逐語により「the application owns the returned
/// file descriptor」なので、返した記述を import に食わせるか閉じるかは呼び手の責任。
///
/// # Safety
///
/// `image` は `context` の device のもので、`VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`
/// で作られ、`memory` が dedicated に束ねられていること。
pub unsafe fn export_dmabuf(
    context: &VulkanContext,
    image: vk::Image,
    memory: vk::DeviceMemory,
    width_px: u32,
    height_px: u32,
    format: DrmFormat,
) -> Result<ExportedDmabuf> {
    let (Some(external_memory_fd), Some(drm)) =
        (context.external_memory_fd(), context.drm_format_modifier())
    else {
        return Err(Error::new(
            ErrorCode::CapabilityUnavailable,
            "this Vulkan device cannot export a dma_buf",
            TraceId::unattributed(),
        ));
    };
    let mut modifier_properties = vk::ImageDrmFormatModifierPropertiesEXT::default();
    // SAFETY: 呼び手の契約により image はこの device の DRM tiling image。
    unsafe { drm.get_image_drm_format_modifier_properties(image, &mut modifier_properties) }
        .map_err(|e| vulkan_failure("vkGetImageDrmFormatModifierPropertiesEXT failed", e))?;
    let modifier = DrmModifier::from_u64(modifier_properties.drm_format_modifier);

    let plane_count = query_modifiers(
        context,
        format.to_vk_format().ok_or_else(|| {
            Error::new(
                ErrorCode::Unsupported,
                "this DRM fourcc has no Vulkan format in the v1 mapping",
                TraceId::unattributed(),
            )
        })?,
        vk::FormatFeatureFlags::empty(),
    )?
    .into_iter()
    .find(|(m, _)| *m == modifier)
    .map(|(_, planes)| planes)
    .ok_or_else(|| {
        Error::new(
            ErrorCode::Internal,
            "the modifier the driver chose is not in the queried modifier list",
            TraceId::unattributed(),
        )
        .with_detail("modifier", modifier.as_u64() as i64)
    })?;

    let aspects = [
        vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_2_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_3_EXT,
    ];
    if plane_count as usize > aspects.len() {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "the chosen DRM format modifier has more memory planes than v1 handles",
            TraceId::unattributed(),
        )
        .with_detail("plane_count", i64::from(plane_count)));
    }
    let mut layouts = Vec::with_capacity(plane_count as usize);
    for aspect in aspects.iter().take(plane_count as usize) {
        // SAFETY: 呼び手の契約により image はこの device のもの。DRM tiling の
        // image では aspectMask に MEMORY_PLANE_n_EXT を渡すのが逐語どおりの形。
        let layout = unsafe {
            context.device().get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: *aspect,
                    mip_level: 0,
                    array_layer: 0,
                },
            )
        };
        layouts.push(layout);
    }

    let get_fd = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    // SAFETY: memory は呼び手が渡した、この device の export 可能な memory。
    let mut planes = Vec::with_capacity(layouts.len());
    for layout in &layouts {
        let raw = unsafe { external_memory_fd.get_memory_fd(&get_fd) }
            .map_err(|e| vulkan_failure("vkGetMemoryFdKHR failed", e))?;
        // SAFETY: raw は vkGetMemoryFdKHR が作った新しい fd で、逐語により
        // その所有は application にある。ここで Rust の所有型へ包む。
        let fd = unsafe {
            use std::os::fd::FromRawFd as _;
            std::os::fd::OwnedFd::from_raw_fd(raw)
        };
        planes.push(DmabufPlane::new(fd, layout.offset, layout.row_pitch));
    }

    Ok(ExportedDmabuf {
        descriptor: DmabufDescriptor {
            width_px,
            height_px,
            format,
            modifier,
            planes,
        },
        chosen_modifier: modifier,
    })
}

/// `fourcc_code` の逐語どおりの計算。
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// `close(2)`。
///
/// # Safety
///
/// `fd` は呼び手が所有しており、他の誰も閉じていないこと。
unsafe fn libc_close(fd: i32) -> i32 {
    unsafe extern "C" {
        fn close(fd: i32) -> i32;
    }
    // SAFETY: 呼び手の契約により fd の所有はここにある。
    unsafe { close(fd) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_matches_the_drm_header_verbatim() {
        // drm_fourcc.h の逐語: DRM_FORMAT_XRGB8888 = fourcc_code('X','R','2','4')
        assert_eq!(DrmFormat::XRGB8888.as_fourcc_string(), "XR24");
        assert_eq!(DrmFormat::ARGB8888.as_fourcc_string(), "AR24");
        assert_eq!(DrmFormat::XRGB8888.as_u32(), 0x3432_5258);
        assert_eq!(DrmFormat::ARGB8888.as_u32(), 0x3432_5241);
    }

    #[test]
    fn linear_modifier_is_vendor_none_value_zero() {
        // 逐語: DRM_FORMAT_MOD_LINEAR = fourcc_mod_code(NONE, 0), VENDOR_NONE = 0
        assert_eq!(DrmModifier::LINEAR.as_u64(), 0);
        assert_eq!(DrmModifier::LINEAR.vendor(), 0);
    }

    #[test]
    fn both_pinned_pixel_formats_have_a_vulkan_format() {
        // 退路 (shm) と import で同じ二枝を扱えること。片方だけ通る形にしない。
        for format in [PixelFormat::Xrgb8888, PixelFormat::Argb8888] {
            let drm = DrmFormat::from_pixel_format(format);
            assert!(drm.to_vk_format().is_some(), "{format:?} has no VkFormat");
        }
    }

    #[test]
    fn an_unknown_fourcc_has_no_vulkan_format() {
        // 知らない fourcc を黙って何かへ丸めない。
        let unknown = DrmFormat(fourcc(b'N', b'V', b'1', b'2'));
        assert!(unknown.to_vk_format().is_none());
    }
}

/// 提出側の dmabuf を自分で作り、それを import し直した `VkImage` を返す。
///
/// **測定のためだけに在る。** 実際の Wayland クライアントは別 lane が持つので、
/// import 経路が成立するかをこの機で測るには提出側が要る。やることは
/// 「クライアントが `zwp_linux_dmabuf_v1` で出してくる物と同じ形の dmabuf を
/// 同じ device で起こし、`vkGetMemoryFdKHR` で取り出し、受け側として
/// [`DmabufImage::import`] で受け直す」である。受け側の経路は本番と同一で、
/// 差し替わるのは提出側だけである。
///
/// 返るのは import した image と、提出側の image / memory である。提出側は
/// import した image が生きている間は返せない (同じ dmabuf を支えている) ので、
/// 呼び手が [`MeasuredDmabuf::release`] で明示的に返す。
pub fn import_own_export_for_measurement(
    context: &VulkanContext,
    frame: &schorl_core::frame::Frame,
) -> Result<MeasuredDmabuf> {
    let drm = DrmFormat::from_pixel_format(frame.format());
    let vk_format = drm.to_vk_format().ok_or_else(|| {
        Error::new(
            ErrorCode::Unsupported,
            "this pixel format has no Vulkan format in the v1 mapping",
            TraceId::unattributed(),
        )
        .with_detail("fourcc", drm.as_fourcc_string())
    })?;
    let modifiers: Vec<u64> = query_modifiers(
        context,
        vk_format,
        vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::TRANSFER_DST,
    )?
    .into_iter()
    // 面が一つの配置だけ。複数面の配置は v1 の import が扱わない。
    .filter(|(_, planes)| *planes == 1)
    .map(|(m, _)| m.as_u64())
    .collect();
    if modifiers.is_empty() {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "no single-plane sampleable DRM format modifier is available for this fourcc",
            TraceId::unattributed(),
        )
        .with_detail("fourcc", drm.as_fourcc_string()));
    }

    let device = context.device().clone();
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
        .format(vk_format)
        .extent(vk::Extent3D {
            width: frame.width_px(),
            height: frame.height_px(),
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
    let producer_image = unsafe { device.create_image(&info, None) }
        .map_err(|e| vulkan_failure("vkCreateImage for the measurement producer failed", e))?;

    let finish = || -> Result<(vk::DeviceMemory, DmabufImage)> {
        // SAFETY: producer_image は直前に作ったもの。
        let requirements = unsafe { device.get_image_memory_requirements(producer_image) };
        let memory_type_index = context.find_memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(producer_image);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut export)
            .push_next(&mut dedicated);
        // SAFETY: allocate の pNext 鎖はこの場の借用。
        let producer_memory = unsafe { device.allocate_memory(&allocate, None) }.map_err(|e| {
            vulkan_failure("vkAllocateMemory for the measurement producer failed", e)
        })?;
        let bound = || -> Result<DmabufImage> {
            // SAFETY: image と memory はこの関数の組。
            unsafe { device.bind_image_memory(producer_image, producer_memory, 0) }.map_err(
                |e| vulkan_failure("vkBindImageMemory for the measurement producer failed", e),
            )?;
            // 提出側が「描いた」ことにあたる書き込み。
            crate::texture::stage_pixels_into_image(
                context,
                producer_image,
                frame,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            )?;
            // SAFETY: producer_image は DRM tiling で作り、memory を dedicated に
            // 束ねた。export の条件を満たしている。
            let exported = unsafe {
                export_dmabuf(
                    context,
                    producer_image,
                    producer_memory,
                    frame.width_px(),
                    frame.height_px(),
                    drm,
                )
            }?;
            // 受け側。ここから先は本番と同じ道である。
            DmabufImage::import(context, exported.descriptor)
        };
        match bound() {
            Ok(imported) => Ok((producer_memory, imported)),
            Err(e) => {
                // SAFETY: producer_memory はこの関数が確保したもの。
                unsafe { device.free_memory(producer_memory, None) };
                Err(e)
            }
        }
    };
    match finish() {
        Ok((producer_memory, imported)) => Ok(MeasuredDmabuf {
            imported,
            producer_image,
            producer_memory,
        }),
        Err(e) => {
            // SAFETY: producer_image はこの関数が作ったもの。
            unsafe { device.destroy_image(producer_image, None) };
            Err(e)
        }
    }
}

/// [`import_own_export_for_measurement`] の結果。
///
/// 提出側と受け側の両方を持つ。提出側は受け側より長く生きなければならないので
/// (同じ dmabuf を支えているため)、返すのは [`MeasuredDmabuf::release`] だけにして
/// 順序を型で守る。
#[derive(Debug)]
pub struct MeasuredDmabuf {
    imported: DmabufImage,
    producer_image: vk::Image,
    producer_memory: vk::DeviceMemory,
}

impl MeasuredDmabuf {
    /// 受け側の `VkImage`。テクスチャを作るのに渡す。
    ///
    /// 呼び手はこれを [`crate::Texture::from_dmabuf`] に渡し、テクスチャを
    /// 落としてから [`MeasuredDmabuf::release`] を呼ぶ。
    pub fn into_imported(self) -> (DmabufImage, ProducerHandles) {
        (
            self.imported,
            ProducerHandles {
                image: self.producer_image,
                memory: self.producer_memory,
            },
        )
    }

    /// 受け側を見る (寸法や modifier を報告に貼るため)。
    pub const fn imported(&self) -> &DmabufImage {
        &self.imported
    }

    /// 両方まとめて返す。
    ///
    /// # Safety
    ///
    /// device の作業が終わっていること。
    pub unsafe fn release(self, context: &VulkanContext) {
        let (imported, producer) = self.into_imported();
        drop(imported);
        // SAFETY: 呼び手の契約により使用中でない。
        unsafe { producer.release(context) };
    }
}

/// 提出側だけを持つ handle。受け側を落としたあとに返す。
#[derive(Debug)]
pub struct ProducerHandles {
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl ProducerHandles {
    /// 返す。
    ///
    /// # Safety
    ///
    /// 受け側 ([`DmabufImage`]) を先に落としていること。device の作業が
    /// 終わっていること。
    pub unsafe fn release(self, context: &VulkanContext) {
        let device = context.device();
        // SAFETY: 呼び手の契約により受け側は落ちており、使用中でない。
        unsafe {
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}
