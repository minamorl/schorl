//! `schorl-render` — 絵を出す側。
//!
//! この crate が満たす pin (`spec-system/pins/domains/schorl.spec@0.2` と、そこが
//! import する `house_style@4.0` → `prohibitions@1.0`):
//!
//! - `space.background: require schorl.virtual_space.background = black` —
//!   黒は [`scope::Background`](schorl_scope::Background) の唯一の枝であり、
//!   [`Renderer`] は毎フレーム swapchain の全画素をその色で clear する。
//!   **ランタイム既定の void に任せていない。** 逐語の根拠は [`space`] の冒頭。
//! - `space.extent: require schorl.virtual_space.extent = 360_degrees` —
//!   持つ層は projection 層一種だけである。projection 層は view の錐台を丸ごと
//!   埋めるので、頭がどちらを向いても自分の黒が出る。方角で層を切り替える枝が無い。
//! - `space.content_unit = window` / `v1.window_count = one_or_more` —
//!   [`Renderer::draw_views`] が受けるのは [`Surface`] の**スライス**である。
//!   一枚に絞る型も定数も無い (上限一枚を再導入しない最弱の形)。
//! - `v1.window_displayed: require schorl.v1.toplevel_window = displayed_in_virtual_space` —
//!   [`Surface`] は姿勢と寸法とテクスチャを持つ矩形で、空間の中へ描かれる。
//! - `v1.scope_excluded: forbid schorl.v1.scope = { curved_surface, ... }` —
//!   面は平面四隅の triangle strip しかない。曲げる媒介変数が型に無い。
//! - `ux.window_not_head_locked: forbid schorl.window.pose = head_locked` —
//!   [`Surface::pose`] は参照空間 (STAGE / LOCAL) の姿勢であり、view の姿勢から
//!   引く経路が無い。`VIEW` 参照空間を作る口も無い。
//! - `verify.machine_scope` の `client_frame_reaches_swapchain` —
//!   [`RenderFacts`] が「何枚 submit したか」「どの経路のテクスチャだったか」を
//!   数字で持つ。要約ではなく観測を報告へ貼れるようにするために在る。
//! - `verify.no_green_substitute` / `verify.hmd_gate` — この crate は HMD を
//!   被った受け入れを一切主張しない。[`RenderFacts`] に「見えた」を表す欄は無い。
//! - `host.created_resource_lifecycle` — 所有した Vulkan 資源は `Drop` で返す。
//!   [`VulkanContext`] / [`Texture`] / [`Renderer`] / [`XrVulkanSession`] の四つ。
//! - `house_style` の横断面: 失敗は [`schorl_core::Error`] の封筒、ログは
//!   [`schorl_core::log`] の json 4 欄、時刻は UTC、待ち直しは
//!   [`schorl_core::retry::Backoff::ExponentialJitter`]。秘密は扱わない。
//!
//! # spec が自由にしている軸で選んだもの
//!
//! `free schorl.compositor.buffer_import_path` と
//! `free schorl.compositor.swapchain_binding` は pin されていない。この crate は
//! そこを次のように埋めている。埋めた事実そのものは pin ではない。
//!
//! - buffer import: `VK_EXT_external_memory_dma_buf` と
//!   `VK_EXT_image_drm_format_modifier` で dmabuf を `VkImage` として import する
//!   ([`dmabuf`])。使えない場面の退路として shm 相当の CPU 画素を staging buffer
//!   経由で `VkImage` へ載せる経路も持つ ([`Texture::upload_frame`])。
//!   **どちらか一方しか無い構成にしない。** 片方が不成立のとき何も出ないのを避ける。
//! - swapchain binding: `XR_KHR_vulkan_enable2`。ランタイムが確保した swapchain
//!   画像へ描き込む。import した `VkImage` を swapchain へ差し込む零コピーは
//!   買わない。理由は一次資料の逐語で、[`xr`] の冒頭に引いてある。
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod dmabuf;
pub mod facts;
pub mod renderer;
pub mod space;
pub mod texture;
pub mod vulkan;
pub mod xr;

pub use dmabuf::{
    DmabufDescriptor, DmabufImage, DmabufPlane, DrmFormat, DrmModifier, ExportableImage,
    ExportedDmabuf,
};
pub use facts::{RenderFacts, StdoutLogSink, TextureRoute};
pub use renderer::{RenderTarget, Renderer};
pub use space::{Mat4, Surface, SurfaceDraw, SurfaceSize, ViewProjection};
pub use texture::Texture;
pub use vulkan::{VulkanContext, VulkanDeviceFacts};
pub use xr::{XrVulkanFacts, XrVulkanRuntime, XrVulkanSession};
