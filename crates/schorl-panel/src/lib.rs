//! `schorl-panel` — 仮想空間に浮く板一枚の幾何。
//!
//! 周囲効果を一つも持たない。ここに在るのは純粋な関数と値だけなので、
//! HMD が無くても全部試験できる (`verify.machine_scope` の `unit_tests`)。
//!
//! 満たす pin: `v1.panel_count` / `v1.panel_grab` / `v1.cursor_moves` /
//! `ux.panel_not_head_locked` / `ux.cursor_mapping` / `ux.cursor_beyond_edge`。

pub mod cursor;
pub mod grab;
pub mod math;
pub mod panel;

pub use cursor::{CursorResolution, PanelPoint, PixelPoint, PointerHold};
pub use grab::{ControllerId, GrabState};
pub use math::{Pose, Quat, Vec3};
pub use panel::{Panel, PanelPose, PanelResolution, PanelSize, PoseFrame};
