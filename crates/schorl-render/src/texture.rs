//! サーフェスへ貼る一枚のテクスチャ。
//!
//! 二つの経路がここで一つの型に合流する。
//!
//! - dmabuf: [`Texture::from_dmabuf`] — [`crate::DmabufImage`] を借りて view と
//!   sampler だけを足す。画素の複製はしない。
//! - shm 相当の CPU 画素: [`Texture::upload_frame`] — staging buffer 経由で
//!   `VkImage` へ載せる。
//!
//! **両方を必ず持つ。** dmabuf が使えない場面で何も出ないのを避けるための退路で
//! あって、飾りではない。どちらで出したかは [`Texture::route`] が持ち、
//! [`crate::RenderFacts`] がそれを数える。

use ash::vk;
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::frame::Frame;
use schorl_core::id::TraceId;

use crate::dmabuf::{DmabufImage, DrmFormat};
use crate::facts::TextureRoute;
use crate::vulkan::{VulkanContext, vulkan_failure};

/// 描く側が sampler から読む一枚。
///
/// `image` の所有は経路によって違う。dmabuf 経路では [`DmabufImage`] が持ち、
/// shm 経路ではこの型が持つ。どちらでも `Drop` で自分が作った物だけを返す
/// (`house.resource_lifecycle.explicit_escape` — 借り物を勝手に壊さない)。
pub struct Texture {
    device: ash::Device,
    /// shm 経路でこの型が作った image。dmabuf 経路では `None`。
    owned_image: Option<vk::Image>,
    /// shm 経路でこの型が確保した memory。dmabuf 経路では `None`。
    owned_memory: Option<vk::DeviceMemory>,
    /// dmabuf 経路で借りている image。落とすとこちらが先に閉じる。
    imported: Option<DmabufImage>,
    image: vk::Image,
    view: vk::ImageView,
    sampler: vk::Sampler,
    width_px: u32,
    height_px: u32,
    route: TextureRoute,
}

impl core::fmt::Debug for Texture {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Texture")
            .field("route", &self.route)
            .field("width_px", &self.width_px)
            .field("height_px", &self.height_px)
            .finish_non_exhaustive()
    }
}

impl Texture {
    /// import した dmabuf を貼れる形にする。
    ///
    /// image layout は `VK_IMAGE_LAYOUT_UNDEFINED` で作られているので、
    /// ここで一度だけ `SHADER_READ_ONLY_OPTIMAL` へ遷移させる。
    /// queue family の所有移転 (`VK_QUEUE_FAMILY_FOREIGN_EXT`) は使っていない:
    /// 逐語で release 側の barrier が対になっている必要があり、提出側が
    /// Vulkan である保証が無いので、`UNDEFINED` からの遷移で受ける。
    pub fn from_dmabuf(context: &VulkanContext, imported: DmabufImage) -> Result<Self> {
        let device = context.device().clone();
        let image = imported.image();
        let view = create_view(&device, image, imported.format())?;
        let sampler = match create_sampler(&device) {
            Ok(sampler) => sampler,
            Err(e) => {
                // SAFETY: view はこの関数が作ったもの。
                unsafe { device.destroy_image_view(view, None) };
                return Err(e);
            }
        };
        let width_px = imported.width_px();
        let height_px = imported.height_px();
        let texture = Self {
            device,
            owned_image: None,
            owned_memory: None,
            imported: Some(imported),
            image,
            view,
            sampler,
            width_px,
            height_px,
            route: TextureRoute::Dmabuf,
        };
        transition_to_shader_read(context, image)?;
        Ok(texture)
    }

    /// CPU 側の画素 (shm 相当) を `VkImage` へ載せる。
    ///
    /// `Frame` は `schorl-core` の型で、寸法と stride の整合はそちらが検めている。
    /// この関数は「dmabuf が使えないときに何も出ない」を避けるための退路である。
    pub fn upload_frame(context: &VulkanContext, frame: &Frame) -> Result<Self> {
        let drm = DrmFormat::from_pixel_format(frame.format());
        let format = drm.to_vk_format().ok_or_else(|| {
            Error::new(
                ErrorCode::Unsupported,
                "this pixel format has no Vulkan format in the v1 mapping",
                TraceId::unattributed(),
            )
            .with_detail("fourcc", drm.as_fourcc_string())
        })?;
        let device = context.device().clone();
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: frame.width_px(),
                height: frame.height_px(),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: image_info はこの場の値。
        let image = unsafe { device.create_image(&image_info, None) }
            .map_err(|e| vulkan_failure("vkCreateImage for the shm fallback failed", e))?;

        let build = || -> Result<(vk::DeviceMemory, vk::ImageView, vk::Sampler)> {
            // SAFETY: image は直前に作ったもの。
            let requirements = unsafe { device.get_image_memory_requirements(image) };
            let memory_type_index = context.find_memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )?;
            let allocate = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type_index);
            // SAFETY: allocate はこの場の値。
            let memory = unsafe { device.allocate_memory(&allocate, None) }
                .map_err(|e| vulkan_failure("vkAllocateMemory for the shm fallback failed", e))?;
            // SAFETY: image と memory はこの関数の組。
            if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
                // SAFETY: memory はこの関数が確保したもの。
                unsafe { device.free_memory(memory, None) };
                return Err(vulkan_failure(
                    "vkBindImageMemory for the shm fallback failed",
                    e,
                ));
            }
            let view = match create_view(&device, image, format) {
                Ok(view) => view,
                Err(e) => {
                    // SAFETY: memory はこの関数が確保したもの。
                    unsafe { device.free_memory(memory, None) };
                    return Err(e);
                }
            };
            let sampler = match create_sampler(&device) {
                Ok(sampler) => sampler,
                Err(e) => {
                    // SAFETY: view と memory はこの関数が作ったもの。
                    unsafe {
                        device.destroy_image_view(view, None);
                        device.free_memory(memory, None);
                    }
                    return Err(e);
                }
            };
            Ok((memory, view, sampler))
        };
        let (memory, view, sampler) = match build() {
            Ok(triple) => triple,
            Err(e) => {
                // SAFETY: image はこの関数が作ったもので、memory は束ねていない。
                unsafe { device.destroy_image(image, None) };
                return Err(e);
            }
        };

        let texture = Self {
            device,
            owned_image: Some(image),
            owned_memory: Some(memory),
            imported: None,
            image,
            view,
            sampler,
            width_px: frame.width_px(),
            height_px: frame.height_px(),
            route: TextureRoute::Shm,
        };
        stage_pixels_into_image(
            context,
            image,
            frame,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        )?;
        Ok(texture)
    }

    /// sampler から読む view。
    pub const fn view(&self) -> vk::ImageView {
        self.view
    }

    /// sampler。
    pub const fn sampler(&self) -> vk::Sampler {
        self.sampler
    }

    /// 元の `VkImage`。
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

    /// どちらの経路で載ったか。
    pub const fn route(&self) -> TextureRoute {
        self.route
    }

    /// 借りている dmabuf の記述 (dmabuf 経路のときだけ)。
    pub const fn imported(&self) -> Option<&DmabufImage> {
        self.imported.as_ref()
    }
}

impl Drop for Texture {
    fn drop(&mut self) {
        // SAFETY: この型が作った view / sampler と、shm 経路で確保した
        // image / memory だけを一度ずつ壊す。dmabuf 経路の image は
        // DmabufImage が持っているので触らない (フィールドの drop 順で先に閉じる)。
        unsafe {
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_image_view(self.view, None);
            if let Some(image) = self.owned_image.take() {
                self.device.destroy_image(image, None);
            }
            if let Some(memory) = self.owned_memory.take() {
                self.device.free_memory(memory, None);
            }
        }
    }
}

/// 2D の色 view を一枚作る。
fn create_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    // SAFETY: info はこの場の値で、image は呼び手が同じ device から渡したもの。
    unsafe { device.create_image_view(&info, None) }
        .map_err(|e| vulkan_failure("vkCreateImageView failed", e))
}

/// 線形補間・縁は clamp の sampler。
///
/// `free schorl.window.scale` は pin されていない軸なので、どう拡縮するかは
/// この選択でよい。縁を繰り返さないのは、板の外側へ画素が漏れないため。
fn create_sampler(device: &ash::Device) -> Result<vk::Sampler> {
    let info = vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::LINEAR)
        .min_filter(vk::Filter::LINEAR)
        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .max_lod(0.0);
    // SAFETY: info はこの場の値。
    unsafe { device.create_sampler(&info, None) }
        .map_err(|e| vulkan_failure("vkCreateSampler failed", e))
}

/// `UNDEFINED` から `SHADER_READ_ONLY_OPTIMAL` へ一度だけ遷移させる。
fn transition_to_shader_read(context: &VulkanContext, image: vk::Image) -> Result<()> {
    crate::renderer::with_one_shot_commands(context, |device, cmd| {
        let barrier = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        // SAFETY: cmd は呼び出し元が begin した記録中の command buffer。
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
        Ok(())
    })
}

/// staging buffer 経由で画素を image へ写す。
///
/// `old_layout` から始めて `new_layout` で終える。提出側 (測定のための
/// producer) も同じ道を通るので、この関数が公開されている。
pub fn stage_pixels_into_image(
    context: &VulkanContext,
    image: vk::Image,
    frame: &Frame,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
) -> Result<()> {
    let device = context.device().clone();
    let bytes = frame.pixels();
    let buffer_info = vk::BufferCreateInfo::default()
        .size(bytes.len() as u64)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: buffer_info はこの場の値。
    let buffer = unsafe { device.create_buffer(&buffer_info, None) }
        .map_err(|e| vulkan_failure("vkCreateBuffer for the staging copy failed", e))?;

    let staged = || -> Result<vk::DeviceMemory> {
        // SAFETY: buffer は直前に作ったもの。
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let memory_type_index = context.find_memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        // SAFETY: allocate はこの場の値。
        let memory = unsafe { device.allocate_memory(&allocate, None) }
            .map_err(|e| vulkan_failure("vkAllocateMemory for the staging copy failed", e))?;
        // SAFETY: buffer と memory はこの関数の組。
        if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
            // SAFETY: memory はこの関数が確保したもの。
            unsafe { device.free_memory(memory, None) };
            return Err(vulkan_failure("vkBindBufferMemory failed", e));
        }
        // SAFETY: memory は HOST_VISIBLE で、要求した長さぶん確保されている。
        let mapped = unsafe {
            device.map_memory(memory, 0, bytes.len() as u64, vk::MemoryMapFlags::empty())
        }
        .map_err(|e| {
            // SAFETY: memory はこの関数が確保したもの。
            unsafe { device.free_memory(memory, None) };
            vulkan_failure("vkMapMemory failed", e)
        })?;
        // SAFETY: mapped は bytes.len() 以上の書き込み可能な領域を指す。
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len());
            device.unmap_memory(memory);
        }
        Ok(memory)
    };
    let memory = match staged() {
        Ok(memory) => memory,
        Err(e) => {
            // SAFETY: buffer はこの関数が作ったもの。
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(e);
        }
    };

    let result = crate::renderer::with_one_shot_commands(context, |device, cmd| {
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let to_dst = vk::ImageMemoryBarrier::default()
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        let to_read = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(new_layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        let bytes_per_pixel = frame.format().bytes_per_pixel();
        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(frame.stride_bytes() / bytes_per_pixel)
            .buffer_image_height(frame.height_px())
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: frame.width_px(),
                height: frame.height_px(),
                depth: 1,
            });
        // SAFETY: cmd は記録中の command buffer。buffer と image は生きている。
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            device.cmd_copy_buffer_to_image(
                cmd,
                buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
            );
        }
        Ok(())
    });

    // 成功でも失敗でも staging を返す (`house.resource_lifecycle.release_paths`)。
    // SAFETY: buffer と memory はこの関数が作ったもので、上の submit は
    // with_one_shot_commands の中で待ち終わっている。
    unsafe {
        device.destroy_buffer(buffer, None);
        device.free_memory(memory, None);
    }
    result
}

/// テクスチャを sampler から読み、その結果を CPU の byte 列として返す。
///
/// **import が `VK_SUCCESS` を返しただけでは、提出された画素がこちらへ届いた
/// 証拠にならない。** `pin verify.machine_scope` の `client_frame_reaches_swapchain`
/// は「実フレームが届く」という不変条件なので、実際に fragment 段から読んだ結果を
/// 照合できる形で取り出す口が要る。
///
/// 画素の並びは `format` のまま。`width` / `height` は `texture` の寸法以下であること。
/// 返る byte 数は `width * height * 4`。
pub fn sample_texture_to_host(
    context: &VulkanContext,
    texture: &Texture,
    format: vk::Format,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    use crate::renderer::Renderer;
    use crate::space::{Mat4, SurfaceDraw, ViewProjection};

    if width == 0 || height == 0 {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            "the read-back extent must be non-zero",
            TraceId::unattributed(),
        ));
    }
    let device = context.device().clone();

    // 1. 描き込む先 (色 attachment かつ転送元)。
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    // SAFETY: image_info はこの場の値。
    let dst = unsafe { device.create_image(&image_info, None) }
        .map_err(|e| vulkan_failure("vkCreateImage for the read-back target failed", e))?;

    let run = || -> Result<Vec<u8>> {
        // SAFETY: dst は直前に作ったもの。
        let requirements = unsafe { device.get_image_memory_requirements(dst) };
        let memory_type_index = context.find_memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        // SAFETY: allocate はこの場の値。
        let dst_memory = unsafe { device.allocate_memory(&allocate, None) }
            .map_err(|e| vulkan_failure("vkAllocateMemory for the read-back target failed", e))?;
        let bound = || -> Result<Vec<u8>> {
            // SAFETY: dst と dst_memory はこの関数の組。
            unsafe { device.bind_image_memory(dst, dst_memory, 0) }
                .map_err(|e| vulkan_failure("vkBindImageMemory for the read-back failed", e))?;

            // 2. 一枚だけ描ける renderer。背景は黒 (型が黒しか持たない)。
            let renderer = Renderer::new(context, format, schorl_scope::Background::Black, 1)?;
            // SAFETY: dst はこの device のもので、形式と寸法は上で指定したもの。
            let target = unsafe {
                crate::renderer::RenderTarget::new(&renderer, dst, format, width, height)
            }?;

            // 3. ローカル ±0.5 の板を viewport 全面へ写す行列。
            //    y を反転させるのは、shader の uv が v = 0.5 - y で、Vulkan の
            //    clip space は +Y が下だからである。これで画像の行 0 が
            //    framebuffer の行 0 に落ちる。
            let full_screen = Mat4 {
                columns: [
                    [2.0, 0.0, 0.0, 0.0],
                    [0.0, -2.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [0.0, 0.0, 0.0, 1.0],
                ],
            };
            let view_projection = ViewProjection {
                view: Mat4::IDENTITY,
                projection: full_screen,
            };
            let draws = [SurfaceDraw {
                texture,
                model: Mat4::IDENTITY,
            }];
            let extent = vk::Extent2D { width, height };
            let drawn = renderer.draw_view(&target, extent, view_projection, &draws);
            // SAFETY: draw_view は fence で待ち終えている。
            unsafe { renderer.destroy_targets(vec![target]) };
            drawn?;

            // 4. host へ写す。
            let read_back = copy_image_to_host(
                context,
                dst,
                format,
                width,
                height,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                None,
            )?;
            drop(renderer);
            Ok(read_back)
        };
        let result = bound();
        // SAFETY: dst_memory はこの関数が確保したもので、上の描画は待ち終えている。
        unsafe { device.free_memory(dst_memory, None) };
        result
    };
    let result = run();
    // SAFETY: dst はこの関数が作ったもの。
    unsafe { device.destroy_image(dst, None) };
    result
}

/// image を host の byte 列へ写す。
///
/// `src_layout` から始める。`restore_layout` を渡すと、写し終えたあとその layout へ
/// 戻す。OpenXR の swapchain 画像を覗くときは、ランタイムへ返す前に元の layout へ
/// 戻さなければならないのでこれが要る。
pub fn copy_image_to_host(
    context: &VulkanContext,
    image: vk::Image,
    format: vk::Format,
    width: u32,
    height: u32,
    src_layout: vk::ImageLayout,
    restore_layout: Option<vk::ImageLayout>,
) -> Result<Vec<u8>> {
    let device = context.device().clone();
    let bytes_per_pixel = match format {
        vk::Format::B8G8R8A8_UNORM
        | vk::Format::B8G8R8A8_SRGB
        | vk::Format::R8G8B8A8_UNORM
        | vk::Format::R8G8B8A8_SRGB => 4u64,
        other => {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "the read-back path only knows 32-bit colour formats",
                TraceId::unattributed(),
            )
            .with_detail("vk_format", other.as_raw() as i64));
        }
    };
    let size = u64::from(width) * u64::from(height) * bytes_per_pixel;
    let buffer_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: buffer_info はこの場の値。
    let buffer = unsafe { device.create_buffer(&buffer_info, None) }
        .map_err(|e| vulkan_failure("vkCreateBuffer for the read-back failed", e))?;

    let run = || -> Result<Vec<u8>> {
        // SAFETY: buffer は直前に作ったもの。
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let memory_type_index = context.find_memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        // SAFETY: allocate はこの場の値。
        let memory = unsafe { device.allocate_memory(&allocate, None) }
            .map_err(|e| vulkan_failure("vkAllocateMemory for the read-back failed", e))?;
        let bound = || -> Result<Vec<u8>> {
            // SAFETY: buffer と memory はこの関数の組。
            unsafe { device.bind_buffer_memory(buffer, memory, 0) }
                .map_err(|e| vulkan_failure("vkBindBufferMemory for the read-back failed", e))?;
            crate::renderer::with_one_shot_commands(context, |device, cmd| {
                let range = vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                let to_src = vk::ImageMemoryBarrier::default()
                    .old_layout(src_layout)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(range)
                    .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
                let region = vk::BufferImageCopy::default()
                    .buffer_offset(0)
                    .buffer_row_length(width)
                    .buffer_image_height(height)
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    });
                // SAFETY: cmd は記録中の command buffer。
                unsafe {
                    device.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_src],
                    );
                    device.cmd_copy_image_to_buffer(
                        cmd,
                        image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        buffer,
                        &[region],
                    );
                }
                if let Some(restore) = restore_layout {
                    let back = vk::ImageMemoryBarrier::default()
                        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .new_layout(restore)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(image)
                        .subresource_range(range)
                        .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
                    // SAFETY: cmd は記録中の command buffer。
                    unsafe {
                        device.cmd_pipeline_barrier(
                            cmd,
                            vk::PipelineStageFlags::TRANSFER,
                            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                            vk::DependencyFlags::empty(),
                            &[],
                            &[],
                            &[back],
                        );
                    }
                }
                Ok(())
            })?;
            // SAFETY: memory は HOST_VISIBLE で size ぶん確保されている。
            let mapped = unsafe { device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) }
                .map_err(|e| vulkan_failure("vkMapMemory for the read-back failed", e))?;
            let mut out = vec![0u8; size as usize];
            // SAFETY: mapped は size ぶんの読み取り可能な領域を指す。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    mapped.cast::<u8>(),
                    out.as_mut_ptr(),
                    size as usize,
                );
                device.unmap_memory(memory);
            }
            Ok(out)
        };
        let result = bound();
        // SAFETY: memory はこの関数が確保したもので、待ちは終わっている。
        unsafe { device.free_memory(memory, None) };
        result
    };
    let result = run();
    // SAFETY: buffer はこの関数が作ったもの。
    unsafe { device.destroy_buffer(buffer, None) };
    result
}
