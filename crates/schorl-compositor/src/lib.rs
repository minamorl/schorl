//! schorl 自身が Wayland compositor になる面。
//!
//! # この crate が満たす pin
//!
//! - `wm.role: require schorl.role = wayland_compositor`
//!   — schorl は他の compositor の画面を借りるのではなく、自分が compositor になる。
//! - `wm.own_socket: require schorl.wayland.socket = owned_by_schorl`
//!   — [`socket`] が自分のソケットを名前ごと所有する。クライアントは
//!   `WAYLAND_DISPLAY=<その名前>` で来る。
//! - `wm.input_ownership: require schorl.input.seat = owned_by_schorl`
//!   — [`state::SchorlCompositor`] が `wl_seat` を持ち、[`pointer`] と [`keyboard`]
//!   がクライアントへ直接配る。注入層を挟まない。
//! - `wm.host_compositor_coexistence: forbid schorl.host_compositor.session = replaced`
//!   — ソケットは自分で新しく取る。宿主の `WAYLAND_DISPLAY` を奪わない。
//! - `host.no_persistent_config_change` / `host.created_resource_lifecycle`
//!   — 作る宿主資源はソケットファイルだけで、[`socket::OwnedSocket`] が drop で返す。
//! - `v1.window_count: require schorl.v1.window.count = one_or_more`
//!   — [`window::WindowRegistry`] に上限を置かない。
//! - `ux.cursor_mapping` / `ux.cursor_beyond_edge`
//!   — [`pointer`] が `schorl-panel` の平面射影をそのまま使い、掴んでいる間は
//!   縁の外でも同じウィンドウへ配り続ける。
//! - `code.log.format` / `code.log.required_fields` / `code.error.envelope`
//!   — ログは [`journal`]、失敗は `schorl_core::Error` の封筒で出す。
//!
//! # この crate が決めないこと
//!
//! dmabuf を GPU テクスチャへ実際に取り込むのは別の面の仕事である。ここは
//! [`buffer::ClientTextureImporter`] という口を開けて引き渡すところまでを持つ
//! (`house.effect_boundary`: 周囲効果は最小の capability にして境界で注入する)。

pub mod buffer;
pub mod driver;
pub mod journal;
pub mod keyboard;
pub mod pointer;
pub mod socket;
pub mod state;
pub mod window;

#[cfg(feature = "nested-winit")]
pub mod nested;

pub use buffer::{
    ClientBufferKind, ClientTextureImporter, DmabufHandoff, RecordingImporter, ShmBufferView,
};
pub use journal::Journal;
pub use socket::{OwnedSocket, SocketName};
pub use state::{CompositorClientData, SchorlCompositor};
pub use window::{AtlasPoint, AtlasRect, WindowId, WindowRegistry};

pub use driver::{HeadlessOptions, HeadlessOutcome, run_headless};
pub use keyboard::EVDEV_TO_XKB_OFFSET;
#[cfg(feature = "nested-winit")]
pub use nested::{NestedOptions, NestedOutcome, run as run_nested};
pub use pointer::PointerDelivery;
pub use state::CompositorSetup;
