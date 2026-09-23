//! 台帳のウィンドウを、描画面が受け取れる [`Surface`] の並びへ組み替える面。
//!
//! `pin v1.window_displayed: require schorl.v1.toplevel_window = displayed_in_virtual_space`
//! の実体はここである。`schorl-render` は「姿勢と寸法とテクスチャを持つ矩形」を
//! 描けるが、その矩形が **toplevel であること** はあちらの知るところではない。
//! 繋いでいるのはこの module だけで、繋がっていなければ描かれるのは
//! 「誰かが作った絵」にすぎない。
//!
//! `pin v1.window_count = one_or_more` に上限は無いので、ここにも枚数の定数を
//! 置かない。descriptor pool の確保量 (`XrVulkanRuntime::with_max_surfaces`) は
//! 資源の量であって仕様の上限ではない。

use schorl_compositor::window::WindowId;
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_render::dmabuf::DmabufImage;
use schorl_render::facts::TextureRoute;
use schorl_render::space::{Surface, SurfaceSize};
use schorl_render::texture::Texture;
use schorl_render::vulkan::VulkanContext;

use crate::grab::WindowPlacement;
use crate::pixels::{ClientBuffer, PixelInbox};

/// 一巡ぶんの組み替えで分かったこと。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageReport {
    /// 新しくテクスチャになったウィンドウの数。
    pub textures_built: usize,
    /// 台帳から消えて外したウィンドウの数。
    pub dropped: usize,
    /// dmabuf 経路で作ったテクスチャの数。
    pub dmabuf_textures: usize,
    /// shm 経路で作ったテクスチャの数。
    pub shm_textures: usize,
}

/// いま空間に立っている矩形の並び。
///
/// テクスチャを所有するので、`VulkanContext` より先に落とすこと。落ちる順序が
/// 逆になると device が無い状態で `vkDestroyImage` を呼ぶ
/// (`house.resource_lifecycle.same_scope`)。
///
/// `keys` と `surfaces` は同じ添字で対応する。描く側は `&[Surface]` を要るので、
/// 並びを連続した一本にしておく必要がある。
#[derive(Debug, Default)]
pub struct Stage {
    keys: Vec<(WindowId, u64)>,
    surfaces: Vec<Surface>,
}

impl Stage {
    /// 空で作る。
    pub const fn new() -> Self {
        Self {
            keys: Vec::new(),
            surfaces: Vec::new(),
        }
    }

    /// 描く側へ渡す並び。
    pub fn surfaces(&self) -> &[Surface] {
        &self.surfaces
    }

    /// 立っている枚数。
    pub fn len(&self) -> usize {
        self.surfaces.len()
    }

    /// 一枚も立っていないか。
    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }

    /// そのウィンドウが立っているか。
    pub fn holds(&self, window: &WindowId) -> bool {
        self.keys.iter().any(|(id, _)| id == window)
    }

    /// そのウィンドウのテクスチャがどちらの経路で来たか。
    pub fn route_of(&self, window: &WindowId) -> Option<TextureRoute> {
        let index = self.keys.iter().position(|(id, _)| id == window)?;
        Some(self.surfaces[index].texture.route())
    }

    /// そのウィンドウがいま立っている姿勢。
    pub fn pose_of(&self, window: &WindowId) -> Option<schorl_panel::math::Pose> {
        let index = self.keys.iter().position(|(id, _)| id == window)?;
        Some(self.surfaces[index].pose)
    }

    /// 台帳といまの受け箱から並びを作り直す。
    ///
    /// - 台帳から消えたウィンドウは外す。
    /// - 新しい絵が来ていればテクスチャを作り直す。
    /// - 姿勢と寸法は毎回書き直す (掴んで動いた結果がここに入る)。
    ///
    /// まだ一度も絵を出していないウィンドウは並びに入らない。**貼る物が無いのに
    /// 矩形だけ立てない。**
    pub fn sync(
        &mut self,
        placements: &[WindowPlacement],
        inbox: &PixelInbox,
        vulkan: &VulkanContext,
    ) -> Result<StageReport> {
        let mut report = StageReport::default();

        let mut index = self.keys.len();
        while index > 0 {
            index -= 1;
            if !placements.iter().any(|p| p.id == self.keys[index].0) {
                self.keys.remove(index);
                self.surfaces.remove(index);
                report.dropped += 1;
            }
        }

        for placement in placements {
            let at = self.keys.iter().position(|(id, _)| *id == placement.id);
            let seen = at.map_or(0, |i| self.keys[i].1);
            let Some(fresh) = inbox.take_newer_than(&placement.id, seen) else {
                continue;
            };
            let (texture, route) = build_texture(vulkan, fresh.buffer)?;
            match route {
                TextureRoute::Dmabuf => report.dmabuf_textures += 1,
                TextureRoute::Shm => report.shm_textures += 1,
            }
            report.textures_built += 1;
            let size = surface_size(placement)?;
            let surface = Surface::new(placement.plane.pose().pose, size, texture);
            match at {
                Some(i) => {
                    self.keys[i].1 = fresh.generation;
                    self.surfaces[i] = surface;
                }
                None => {
                    self.keys.push((placement.id.clone(), fresh.generation));
                    self.surfaces.push(surface);
                }
            }
        }

        // 姿勢と寸法は絵の新しさと関係なく毎回書き直す。掴んで動いた結果が
        // 次の一枚に出るのはここを通るからである。
        for (index, (id, _)) in self.keys.iter().enumerate() {
            let Some(placement) = placements.iter().find(|p| p.id == *id) else {
                continue;
            };
            self.surfaces[index].pose = placement.plane.pose().pose;
            self.surfaces[index].size = surface_size(placement)?;
        }

        Ok(report)
    }

    /// テクスチャを明示的に手放す。
    ///
    /// `Drop` でも落ちるが、`VkDevice` が空になってから落としたい場面がある
    /// (`house.resource_lifecycle.release_paths` の「失敗を報告できる道」)。
    pub fn release(&mut self) {
        self.surfaces.clear();
        self.keys.clear();
    }
}

fn surface_size(placement: &WindowPlacement) -> Result<SurfaceSize> {
    let size = placement.plane.size();
    SurfaceSize::new(size.width_m, size.height_m)
}

fn build_texture(vulkan: &VulkanContext, buffer: ClientBuffer) -> Result<(Texture, TextureRoute)> {
    match buffer {
        ClientBuffer::Shm(frame) => {
            Ok((Texture::upload_frame(vulkan, &frame)?, TextureRoute::Shm))
        }
        ClientBuffer::Dmabuf(descriptor) => {
            if !vulkan.supports_dmabuf_import() {
                return Err(Error::new(
                    ErrorCode::CapabilityUnavailable,
                    "the client handed over a dma_buf but this Vulkan device has no import path",
                    TraceId::unattributed(),
                ));
            }
            let imported = DmabufImage::import(vulkan, descriptor)?;
            Ok((Texture::from_dmabuf(vulkan, imported)?, TextureRoute::Dmabuf))
        }
    }
}
