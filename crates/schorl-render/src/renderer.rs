//! 黒い空間と、そこに置いたサーフェスを実際に描く面。
//!
//! # 何を所有しているか
//!
//! render pass / pipeline / descriptor pool / command pool / fence。すべて
//! `Drop` で返す (`host.created_resource_lifecycle`)。
//!
//! # 黒の持ち主
//!
//! attachment の `loadOp` は `VK_ATTACHMENT_LOAD_OP_CLEAR` で、clear 値は
//! [`schorl_scope::Background::clear_color_rgba`] である。つまり毎フレーム、
//! swapchain 画像の全画素をこちらが黒で書く。`pin space.background` を
//! 「ランタイム既定の void に任せない」形で満たすのはここである。

use ash::vk;
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_scope::Background;

use crate::facts::TextureRoute;
use crate::space::{SurfaceDraw, ViewProjection};
use crate::vulkan::{VulkanContext, vulkan_failure};

/// 描き込む先の一枚。
///
/// OpenXR の swapchain 画像はランタイムが確保した `VkImage` である。
/// **こちらが持ち込んだ画像を swapchain へ差し込む経路は一次資料に無い。**
/// `xrEnumerateSwapchainImages` の逐語:
///
/// > Fills an array of graphics API-specific `XrSwapchainImage` structures.
/// > The resources **must** be constant and valid for the lifetime of the XrSwapchain.
///
/// > Runtimes **must** always return identical buffer contents from this
/// > enumeration for the lifetime of the swapchain.
///
/// よって零コピーは買わず、import した `VkImage` を sampler で読み、ランタイムの
/// 画像へ描き込む。`free schorl.compositor.swapchain_binding` の選択である。
#[derive(Debug)]
pub struct RenderTarget {
    image: vk::Image,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
}

impl RenderTarget {
    /// ランタイムが持つ `VkImage` を借りて view と framebuffer を足す。
    ///
    /// `image` の所有はランタイムに残る。この型は view と framebuffer だけを
    /// 所有し、[`RenderTarget::destroy`] でそれだけを返す。
    ///
    /// # Safety
    ///
    /// `image` は `renderer` の device のもので、`width` / `height` / `format` が
    /// その画像と一致していること。
    pub unsafe fn new(
        renderer: &Renderer,
        image: vk::Image,
        format: vk::Format,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let device = &renderer.device;
        let view_info = vk::ImageViewCreateInfo::default()
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
        // SAFETY: 呼び手の契約により image はこの device のもの。
        let view = unsafe { device.create_image_view(&view_info, None) }
            .map_err(|e| vulkan_failure("vkCreateImageView for the swapchain image failed", e))?;
        let attachments = [view];
        let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(renderer.render_pass)
            .attachments(&attachments)
            .width(width)
            .height(height)
            .layers(1);
        // SAFETY: render_pass は renderer が所有する生きたもの。
        let framebuffer = match unsafe { device.create_framebuffer(&fb_info, None) } {
            Ok(framebuffer) => framebuffer,
            Err(e) => {
                // SAFETY: view はこの関数が作ったもの。
                unsafe { device.destroy_image_view(view, None) };
                return Err(vulkan_failure("vkCreateFramebuffer failed", e));
            }
        };
        Ok(Self {
            image,
            view,
            framebuffer,
        })
    }

    /// 借りているランタイムの画像。
    pub const fn image(&self) -> vk::Image {
        self.image
    }

    /// この型が作った view と framebuffer を返す。
    ///
    /// `Drop` にしていないのは device を持ち歩かせたくないためで、代わりに
    /// [`Renderer::destroy_targets`] が必ず通る道で呼ぶ。
    ///
    /// # Safety
    ///
    /// `device` は作ったときと同じものであること。device の作業が終わっていること。
    pub unsafe fn destroy(self, device: &ash::Device) {
        // SAFETY: 呼び手の契約により同じ device で、使用中でない。
        unsafe {
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_image_view(self.view, None);
        }
    }
}

/// push constant に載せる量。
///
/// mvp の 16 個だけ。サーフェスの寸法と姿勢はそこに畳んである。
const PUSH_CONSTANT_BYTES: u32 = 16 * 4;

/// 一度に描けるサーフェスの上限。
///
/// これは descriptor pool の確保量であって仕様の上限ではない。
/// `pin v1.window_count = one_or_more` に上限は無いので、足りなければ
/// [`Renderer::new`] に大きい値を渡す。**型として一枚に絞ってはいない。**
pub const DEFAULT_MAX_SURFACES_PER_FRAME: u32 = 32;

/// 描く側。
pub struct Renderer {
    device: ash::Device,
    queue: vk::Queue,
    render_pass: vk::RenderPass,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    color_format: vk::Format,
    background: Background,
    max_surfaces: u32,
}

impl core::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Renderer")
            .field("color_format", &self.color_format)
            .field("background", &self.background)
            .field("max_surfaces", &self.max_surfaces)
            .finish_non_exhaustive()
    }
}

impl Renderer {
    /// 組み立てる。
    ///
    /// `color_format` は swapchain の形式に合わせること。`background` は
    /// [`Background`] なので黒以外を渡せない (`pin space.background`)。
    pub fn new(
        context: &VulkanContext,
        color_format: vk::Format,
        background: Background,
        max_surfaces: u32,
    ) -> Result<Self> {
        if max_surfaces == 0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "the renderer must be able to draw at least one surface",
                TraceId::unattributed(),
            ));
        }
        let device = context.device().clone();
        let render_pass = create_render_pass(&device, color_format)?;
        let mut built = Built {
            device: &device,
            render_pass: Some(render_pass),
            descriptor_layout: None,
            pipeline_layout: None,
            pipeline: None,
            descriptor_pool: None,
            command_pool: None,
            fence: None,
        };
        let descriptor_layout = built.keep_descriptor_layout(create_descriptor_layout(&device)?);
        let pipeline_layout =
            built.keep_pipeline_layout(create_pipeline_layout(&device, descriptor_layout)?);
        let pipeline = built.keep_pipeline(create_pipeline(&device, render_pass, pipeline_layout)?);
        let descriptor_pool =
            built.keep_descriptor_pool(create_descriptor_pool(&device, max_surfaces)?);
        let command_pool =
            built.keep_command_pool(create_command_pool(&device, context.queue_family_index())?);
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .command_buffer_count(1);
        // SAFETY: command_pool は直前に作ったもの。
        let command_buffer = unsafe { device.allocate_command_buffers(&allocate) }
            .map_err(|e| vulkan_failure("vkAllocateCommandBuffers failed", e))?[0];
        // signaled で作る。初回の vkWaitForFences がそのまま返るので、
        // 「初回だけ待たない」という分岐を持たずに済む。
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: fence_info はこの場の値。
        let fence = built.keep_fence(
            unsafe { device.create_fence(&fence_info, None) }
                .map_err(|e| vulkan_failure("vkCreateFence failed", e))?,
        );
        built.disarm();
        // Built は device を借用しているので、device を移す前に手放す。
        drop(built);
        Ok(Self {
            device,
            queue: context.queue(),
            render_pass,
            pipeline_layout,
            pipeline,
            descriptor_layout,
            descriptor_pool,
            command_pool,
            command_buffer,
            fence,
            color_format,
            background,
            max_surfaces,
        })
    }

    /// この renderer が組んだ render pass。
    pub const fn render_pass(&self) -> vk::RenderPass {
        self.render_pass
    }

    /// 背景。
    pub const fn background(&self) -> Background {
        self.background
    }

    /// 一度に描けるサーフェスの上限 (descriptor pool の確保量)。
    pub const fn max_surfaces(&self) -> u32 {
        self.max_surfaces
    }

    /// 借りた view / framebuffer をまとめて返す。
    ///
    /// # Safety
    ///
    /// device の作業が終わっていること。
    pub unsafe fn destroy_targets(&self, targets: Vec<RenderTarget>) {
        for target in targets {
            // SAFETY: 呼び手の契約により使用中でない。device は同じもの。
            unsafe { target.destroy(&self.device) };
        }
    }

    /// 一つの view へ、黒い背景とサーフェスを描く。
    ///
    /// `surfaces` は 0 枚でもよい。0 枚のときも背景の黒は書かれる
    /// (**空間の黒は中身の有無に依らない**)。
    pub fn draw_view(
        &self,
        target: &RenderTarget,
        extent: vk::Extent2D,
        view_projection: ViewProjection,
        surfaces: &[SurfaceDraw<'_>],
    ) -> Result<Vec<TextureRoute>> {
        if surfaces.len() as u32 > self.max_surfaces {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "more surfaces were submitted than the descriptor pool was sized for",
                TraceId::unattributed(),
            )
            .with_detail("submitted", surfaces.len() as i64)
            .with_detail("max_surfaces", i64::from(self.max_surfaces)));
        }
        let device = &self.device;
        // SAFETY: fence と command buffer はこの型が所有している。前の submit が
        // 終わるまで待ってから記録し直す。fence は signaled で作ってあるので
        // 初回も待ちはすぐ返る。
        unsafe {
            device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .map_err(|e| vulkan_failure("vkWaitForFences failed", e))?;
            device
                .reset_fences(&[self.fence])
                .map_err(|e| vulkan_failure("vkResetFences failed", e))?;
        }

        let descriptor_sets = self.allocate_descriptors(surfaces)?;
        let routes: Vec<TextureRoute> = surfaces.iter().map(|s| s.texture.route()).collect();

        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        let clear = self.background.clear_color_rgba();
        let clear_values = [vk::ClearValue {
            color: vk::ClearColorValue { float32: clear },
        }];
        let render_area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        let pass_begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(target.framebuffer)
            .render_area(render_area)
            .clear_values(&clear_values);
        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [render_area];
        let vp = view_projection.view_projection();

        // SAFETY: すべての handle はこの型か target が所有する生きたもの。
        // 記録は begin と end で閉じ、submit してから fence で待つ。
        unsafe {
            device
                .begin_command_buffer(self.command_buffer, &begin)
                .map_err(|e| vulkan_failure("vkBeginCommandBuffer failed", e))?;
            device.cmd_begin_render_pass(
                self.command_buffer,
                &pass_begin,
                vk::SubpassContents::INLINE,
            );
            device.cmd_set_viewport(self.command_buffer, 0, &viewports);
            device.cmd_set_scissor(self.command_buffer, 0, &scissors);
            device.cmd_bind_pipeline(
                self.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline,
            );
            for (surface, set) in surfaces.iter().zip(descriptor_sets.iter()) {
                let mvp = vp.mul(surface.model).to_array();
                let bytes: &[u8] = core::slice::from_raw_parts(
                    mvp.as_ptr().cast::<u8>(),
                    PUSH_CONSTANT_BYTES as usize,
                );
                device.cmd_bind_descriptor_sets(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &[*set],
                    &[],
                );
                device.cmd_push_constants(
                    self.command_buffer,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    bytes,
                );
                // 四隅の triangle strip。曲げる媒介変数が無い
                // (`forbid schorl.v1.scope = { curved_surface, ... }`)。
                device.cmd_draw(self.command_buffer, 4, 1, 0, 0);
            }
            device.cmd_end_render_pass(self.command_buffer);
            device
                .end_command_buffer(self.command_buffer)
                .map_err(|e| vulkan_failure("vkEndCommandBuffer failed", e))?;
            let command_buffers = [self.command_buffer];
            let submit = vk::SubmitInfo::default().command_buffers(&command_buffers);
            device
                .queue_submit(self.queue, &[submit], self.fence)
                .map_err(|e| vulkan_failure("vkQueueSubmit failed", e))?;
            device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .map_err(|e| vulkan_failure("vkWaitForFences after submit failed", e))?;
            device
                .reset_descriptor_pool(self.descriptor_pool, vk::DescriptorPoolResetFlags::empty())
                .map_err(|e| vulkan_failure("vkResetDescriptorPool failed", e))?;
        }
        Ok(routes)
    }

    /// サーフェスの数だけ descriptor set を確保して sampler を書き込む。
    fn allocate_descriptors(&self, surfaces: &[SurfaceDraw<'_>]) -> Result<Vec<vk::DescriptorSet>> {
        if surfaces.is_empty() {
            return Ok(Vec::new());
        }
        let layouts = vec![self.descriptor_layout; surfaces.len()];
        let allocate = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&layouts);
        // SAFETY: pool と layout はこの型が所有する生きたもの。
        let sets = unsafe { self.device.allocate_descriptor_sets(&allocate) }
            .map_err(|e| vulkan_failure("vkAllocateDescriptorSets failed", e))?;
        let images: Vec<[vk::DescriptorImageInfo; 1]> = surfaces
            .iter()
            .map(|surface| {
                [vk::DescriptorImageInfo::default()
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .image_view(surface.texture.view())
                    .sampler(surface.texture.sampler())]
            })
            .collect();
        let writes: Vec<vk::WriteDescriptorSet<'_>> = sets
            .iter()
            .zip(images.iter())
            .map(|(set, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(info)
            })
            .collect();
        // SAFETY: writes が指す image info はこの関数の生きた借用。
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
        Ok(sets)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: この型が所有した物だけを一度ずつ壊す。待ちの失敗は Drop からは
        // 報告できないので黙って進む (報告経路は VulkanContext::wait_idle)。
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.descriptor_layout, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device.destroy_render_pass(self.render_pass, None);
        }
    }
}

/// 組み立ての途中で失敗したときに、そこまでに作った物を巻き戻す入れ物。
///
/// `house.resource_lifecycle.release_paths` は「成功・失敗・早期 return の
/// どの道でも返す」ことを要求している。組み立ては段が多いので、段ごとに
/// 手で巻き戻すと道が増えて漏れる。`Drop` に寄せて一本にする。
struct Built<'a> {
    device: &'a ash::Device,
    render_pass: Option<vk::RenderPass>,
    descriptor_layout: Option<vk::DescriptorSetLayout>,
    pipeline_layout: Option<vk::PipelineLayout>,
    pipeline: Option<vk::Pipeline>,
    descriptor_pool: Option<vk::DescriptorPool>,
    command_pool: Option<vk::CommandPool>,
    fence: Option<vk::Fence>,
}

impl Built<'_> {
    fn keep_descriptor_layout(
        &mut self,
        value: vk::DescriptorSetLayout,
    ) -> vk::DescriptorSetLayout {
        self.descriptor_layout = Some(value);
        value
    }
    fn keep_pipeline_layout(&mut self, value: vk::PipelineLayout) -> vk::PipelineLayout {
        self.pipeline_layout = Some(value);
        value
    }
    fn keep_pipeline(&mut self, value: vk::Pipeline) -> vk::Pipeline {
        self.pipeline = Some(value);
        value
    }
    fn keep_descriptor_pool(&mut self, value: vk::DescriptorPool) -> vk::DescriptorPool {
        self.descriptor_pool = Some(value);
        value
    }
    fn keep_command_pool(&mut self, value: vk::CommandPool) -> vk::CommandPool {
        self.command_pool = Some(value);
        value
    }
    fn keep_fence(&mut self, value: vk::Fence) -> vk::Fence {
        self.fence = Some(value);
        value
    }

    /// 全部そろったので巻き戻さない (所有は呼び手の型へ移る)。
    fn disarm(&mut self) {
        self.render_pass = None;
        self.descriptor_layout = None;
        self.pipeline_layout = None;
        self.pipeline = None;
        self.descriptor_pool = None;
        self.command_pool = None;
        self.fence = None;
    }
}

impl Drop for Built<'_> {
    fn drop(&mut self) {
        // SAFETY: まだ誰にも渡していない物だけを壊す。disarm 後は空なので
        // 何も壊さない。
        unsafe {
            if let Some(v) = self.fence.take() {
                self.device.destroy_fence(v, None);
            }
            if let Some(v) = self.command_pool.take() {
                self.device.destroy_command_pool(v, None);
            }
            if let Some(v) = self.descriptor_pool.take() {
                self.device.destroy_descriptor_pool(v, None);
            }
            if let Some(v) = self.pipeline.take() {
                self.device.destroy_pipeline(v, None);
            }
            if let Some(v) = self.pipeline_layout.take() {
                self.device.destroy_pipeline_layout(v, None);
            }
            if let Some(v) = self.descriptor_layout.take() {
                self.device.destroy_descriptor_set_layout(v, None);
            }
            if let Some(v) = self.render_pass.take() {
                self.device.destroy_render_pass(v, None);
            }
        }
    }
}

/// 一枚の色 attachment だけの render pass。
///
/// `loadOp = CLEAR` が「黒を自分で書く」ことの実体である。
fn create_render_pass(device: &ash::Device, color_format: vk::Format) -> Result<vk::RenderPass> {
    let attachments = [vk::AttachmentDescription {
        format: color_format,
        samples: vk::SampleCountFlags::TYPE_1,
        load_op: vk::AttachmentLoadOp::CLEAR,
        store_op: vk::AttachmentStoreOp::STORE,
        stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
        stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
        initial_layout: vk::ImageLayout::UNDEFINED,
        final_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        ..Default::default()
    }];
    let color_refs = [vk::AttachmentReference {
        attachment: 0,
        layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
    }];
    let subpasses = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs)];
    let dependencies = [vk::SubpassDependency {
        src_subpass: vk::SUBPASS_EXTERNAL,
        dst_subpass: 0,
        src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
        dst_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
        dst_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        ..Default::default()
    }];
    let info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&dependencies);
    // SAFETY: info はこの場の値。
    unsafe { device.create_render_pass(&info, None) }
        .map_err(|e| vulkan_failure("vkCreateRenderPass failed", e))
}

/// sampler 一つだけの descriptor set layout。
fn create_descriptor_layout(device: &ash::Device) -> Result<vk::DescriptorSetLayout> {
    let bindings = [vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
    // SAFETY: info はこの場の値。
    unsafe { device.create_descriptor_set_layout(&info, None) }
        .map_err(|e| vulkan_failure("vkCreateDescriptorSetLayout failed", e))
}

/// mvp の push constant と sampler の set を持つ layout。
fn create_pipeline_layout(
    device: &ash::Device,
    descriptor_layout: vk::DescriptorSetLayout,
) -> Result<vk::PipelineLayout> {
    let set_layouts = [descriptor_layout];
    let ranges = [vk::PushConstantRange {
        stage_flags: vk::ShaderStageFlags::VERTEX,
        offset: 0,
        size: PUSH_CONSTANT_BYTES,
    }];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(&ranges);
    // SAFETY: info はこの場の値。
    unsafe { device.create_pipeline_layout(&info, None) }
        .map_err(|e| vulkan_failure("vkCreatePipelineLayout failed", e))
}

/// 板一枚を貼る pipeline。
///
/// 頂点バッファを持たない。四隅は shader が `gl_VertexIndex` から引く。
/// topology は `TRIANGLE_STRIP` で、`cmd_draw(4, ...)` が二枚の三角形になる。
fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
) -> Result<vk::Pipeline> {
    // .spv は build script が shaders/*.vert / *.frag から OUT_DIR へ焼く。
    // repo には source だけを置く (pin public.no_build_artifacts)。
    let vert_spv = include_bytes!(concat!(env!("OUT_DIR"), "/quad.vert.spv"));
    let frag_spv = include_bytes!(concat!(env!("OUT_DIR"), "/quad.frag.spv"));
    let vert = create_shader_module(device, vert_spv)?;
    let frag = match create_shader_module(device, frag_spv) {
        Ok(frag) => frag,
        Err(e) => {
            // SAFETY: vert はこの関数が作ったもの。
            unsafe { device.destroy_shader_module(vert, None) };
            return Err(e);
        }
    };

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag)
            .name(entry),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
        // 板の裏からも見える。360 度の空間では回り込めるので背面を捨てない。
        .cull_mode(vk::CullModeFlags::NONE)
        .polygon_mode(vk::PolygonMode::FILL)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let noop_stencil = vk::StencilOpState {
        fail_op: vk::StencilOp::KEEP,
        pass_op: vk::StencilOp::KEEP,
        depth_fail_op: vk::StencilOp::KEEP,
        compare_op: vk::CompareOp::ALWAYS,
        compare_mask: 0,
        write_mask: 0,
        reference: 0,
    };
    let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
        .depth_test_enable(false)
        .depth_write_enable(false)
        .front(noop_stencil)
        .back(noop_stencil);
    let blend_attachments = [vk::PipelineColorBlendAttachmentState {
        blend_enable: vk::FALSE,
        color_write_mask: vk::ColorComponentFlags::R
            | vk::ColorComponentFlags::G
            | vk::ColorComponentFlags::B
            | vk::ColorComponentFlags::A,
        ..Default::default()
    }];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let infos = [vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .depth_stencil_state(&depth_stencil)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0)];
    // SAFETY: infos が指す物はこの関数の生きた借用。
    let created =
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &infos, None) };
    // shader module は pipeline を作ったあとは要らない。成功・失敗どちらでも返す。
    // SAFETY: vert / frag はこの関数が作ったもの。
    unsafe {
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);
    }
    match created {
        Ok(pipelines) => pipelines.into_iter().next().ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "vkCreateGraphicsPipelines returned no pipeline",
                TraceId::unattributed(),
            )
        }),
        Err((_, e)) => Err(vulkan_failure("vkCreateGraphicsPipelines failed", e)),
    }
}

/// SPIR-V の byte 列から shader module を作る。
fn create_shader_module(device: &ash::Device, spv: &[u8]) -> Result<vk::ShaderModule> {
    if !spv.len().is_multiple_of(4) {
        return Err(Error::new(
            ErrorCode::Internal,
            "the embedded SPIR-V is not a whole number of words",
            TraceId::unattributed(),
        )
        .with_detail("bytes", spv.len() as i64));
    }
    let mut words = Vec::with_capacity(spv.len() / 4);
    for chunk in spv.chunks_exact(4) {
        words.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    let info = vk::ShaderModuleCreateInfo::default().code(&words);
    // SAFETY: info が指す words はこの関数の生きた借用。
    unsafe { device.create_shader_module(&info, None) }
        .map_err(|e| vulkan_failure("vkCreateShaderModule failed", e))
}

/// サーフェス n 枚ぶんの sampler を確保できる pool。
fn create_descriptor_pool(device: &ash::Device, max_surfaces: u32) -> Result<vk::DescriptorPool> {
    let sizes = [vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(max_surfaces)];
    let info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(max_surfaces)
        .pool_sizes(&sizes);
    // SAFETY: info はこの場の値。
    unsafe { device.create_descriptor_pool(&info, None) }
        .map_err(|e| vulkan_failure("vkCreateDescriptorPool failed", e))
}

/// 記録し直せる command pool。
fn create_command_pool(device: &ash::Device, queue_family_index: u32) -> Result<vk::CommandPool> {
    let info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(queue_family_index)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    // SAFETY: info はこの場の値。
    unsafe { device.create_command_pool(&info, None) }
        .map_err(|e| vulkan_failure("vkCreateCommandPool failed", e))
}

/// 使い捨ての command buffer を一本回す。
///
/// layout 遷移や staging の copy のような、フレームに属さない一回きりの仕事に使う。
/// 作った pool と buffer は成功・失敗のどちらの道でも返す。
pub(crate) fn with_one_shot_commands<F>(context: &VulkanContext, record: F) -> Result<()>
where
    F: FnOnce(&ash::Device, vk::CommandBuffer) -> Result<()>,
{
    let device = context.device();
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(context.queue_family_index())
        .flags(vk::CommandPoolCreateFlags::TRANSIENT);
    // SAFETY: pool_info はこの場の値。
    let pool = unsafe { device.create_command_pool(&pool_info, None) }
        .map_err(|e| vulkan_failure("vkCreateCommandPool for a one-shot submit failed", e))?;

    let run = || -> Result<()> {
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .command_buffer_count(1);
        // SAFETY: pool は直前に作ったもの。
        let buffers = unsafe { device.allocate_command_buffers(&allocate) }
            .map_err(|e| vulkan_failure("vkAllocateCommandBuffers failed", e))?;
        let cmd = buffers[0];
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: cmd は直前に確保したもの。
        unsafe { device.begin_command_buffer(cmd, &begin) }
            .map_err(|e| vulkan_failure("vkBeginCommandBuffer failed", e))?;
        record(device, cmd)?;
        // SAFETY: 同上。
        unsafe { device.end_command_buffer(cmd) }
            .map_err(|e| vulkan_failure("vkEndCommandBuffer failed", e))?;
        let command_buffers = [cmd];
        let submit = vk::SubmitInfo::default().command_buffers(&command_buffers);
        // SAFETY: queue は context が所有する生きたもの。
        unsafe { device.queue_submit(context.queue(), &[submit], vk::Fence::null()) }
            .map_err(|e| vulkan_failure("vkQueueSubmit for a one-shot submit failed", e))?;
        // SAFETY: 同上。fence を使わないので idle で待つ。
        unsafe { device.queue_wait_idle(context.queue()) }
            .map_err(|e| vulkan_failure("vkQueueWaitIdle failed", e))
    };
    let result = run();
    // SAFETY: pool はこの関数が作ったもので、上の待ちが終わっている。
    unsafe { device.destroy_command_pool(pool, None) };
    result
}
