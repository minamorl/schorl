//! ポインタをクライアントへ配る。座標は surface-local。
//!
//! # なぜ surface-local になるか
//!
//! smithay の `PointerHandle::motion` は「合成空間での点」と「その surface の
//! 原子が合成空間のどこに在るか」の二つを取り、差を `wl_pointer.motion` として
//! 送る。schorl は [`crate::window::AtlasRect`] でウィンドウごとに互いに素な
//! 矩形を割り当てているので、
//!
//! ```text
//!   合成空間の点 = 矩形の原点 + ウィンドウ内の画素
//! ```
//!
//! を渡せば、クライアントが受け取る座標はちょうど「ウィンドウ内の画素」になる。
//!
//! # 平面射影と縁の外
//!
//! `pin ux.cursor_mapping: require schorl.cursor.mapping = window_plane_projection`
//! と `pin ux.cursor_beyond_edge` は `schorl-panel` の
//! [`resolve_cursor`](schorl_panel::cursor::resolve_cursor) がすでに持っている。
//! ここはその結果を Wayland の語へ翻訳するだけで、判定をやり直さない。

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_input::{
    ButtonState, Delivery, PointerButton, PointerEvent, PointerEventKind, PointerSink,
};
use schorl_panel::cursor::{PixelPoint, PointerHold, resolve_cursor, to_pixels};
use schorl_panel::math::Vec3;

use smithay::backend::input::ButtonState as SmithayButtonState;
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::utils::{Logical, Point};

use crate::state::SchorlCompositor;
use crate::window::WindowId;

/// Linux の `input-event-codes.h` が定めるポインタボタン。
///
/// `wl_pointer.button` の `button` はこの値をそのまま運ぶ。
/// 出典: `/usr/include/linux/input-event-codes.h`
/// (`BTN_LEFT 0x110` / `BTN_RIGHT 0x111` / `BTN_MIDDLE 0x112`)。
pub const BTN_LEFT: u32 = 0x110;
/// 右ボタン。
pub const BTN_RIGHT: u32 = 0x111;
/// 中ボタン。
pub const BTN_MIDDLE: u32 = 0x112;

/// [`schorl_input::PointerButton`] を Linux のボタン番号へ直す。
pub const fn evdev_button_code(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => BTN_LEFT,
        PointerButton::Right => BTN_RIGHT,
        PointerButton::Middle => BTN_MIDDLE,
    }
}

const fn to_smithay_state(state: ButtonState) -> SmithayButtonState {
    match state {
        ButtonState::Pressed => SmithayButtonState::Pressed,
        ButtonState::Released => SmithayButtonState::Released,
    }
}

/// 配った結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerDelivery {
    /// このウィンドウの、この surface-local 画素へ動かした。
    Moved {
        /// 宛先。
        window: WindowId,
        /// クライアントが受け取る座標。
        local: PixelPoint,
    },
    /// どのウィンドウの上でもなく、押してもいないので何も動かさなかった。
    ///
    /// このとき `wl_pointer.leave` が出る。
    Left,
}

impl SchorlCompositor {
    /// いまポインタが向いているウィンドウ。
    pub const fn pointer_target(&self) -> Option<&WindowId> {
        self.pointer_target.as_ref()
    }

    /// ポインタの宛先を明示的に決める。
    pub fn set_pointer_target(&mut self, window: Option<WindowId>) {
        self.pointer_target = window;
    }

    /// 世界座標の一点をウィンドウ平面へ射影し、その surface へポインタを配る。
    ///
    /// `hold` が [`PointerHold::Down`] なら、縁を越えても同じウィンドウへ
    /// 配り続ける (`pin ux.cursor_beyond_edge`)。
    pub fn aim_at(
        &mut self,
        window: &WindowId,
        world_point: Vec3,
        hold: PointerHold,
        time_ms: u32,
    ) -> Result<PointerDelivery> {
        let tracked = self.windows.get(window).ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidArgument,
                "no such window is being tracked",
                self.journal.trace_id().clone(),
            )
            .with_detail("window", window.as_str())
        })?;

        let plane = tracked.plane();
        let rect = tracked.rect();
        let surface = tracked.handle().wl_surface().clone();

        match resolve_cursor(&plane, world_point, hold).point() {
            None => {
                self.set_pointer_target(None);
                self.send_motion(None, Point::from((0.0, 0.0)), time_ms)?;
                Ok(PointerDelivery::Left)
            }
            Some(plane_point) => {
                let local = to_pixels(&plane, plane_point);
                let origin: Point<f64, Logical> =
                    Point::from((f64::from(rect.x), f64::from(rect.y)));
                let location: Point<f64, Logical> =
                    Point::from((f64::from(rect.x + local.x), f64::from(rect.y + local.y)));
                self.set_pointer_target(Some(window.clone()));
                self.send_motion(Some((surface, origin)), location, time_ms)?;
                Ok(PointerDelivery::Moved {
                    window: window.clone(),
                    local,
                })
            }
        }
    }

    /// surface-local な画素を直に指定して配る。
    ///
    /// 3D の射影を経ない経路 (入れ子 backend のマウスなど) が使う。
    pub fn point_at_local(
        &mut self,
        window: &WindowId,
        local: PixelPoint,
        time_ms: u32,
    ) -> Result<PointerDelivery> {
        let tracked = self.windows.get(window).ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidArgument,
                "no such window is being tracked",
                self.journal.trace_id().clone(),
            )
            .with_detail("window", window.as_str())
        })?;
        let rect = tracked.rect();
        let surface = tracked.handle().wl_surface().clone();
        let origin: Point<f64, Logical> = Point::from((f64::from(rect.x), f64::from(rect.y)));
        let location: Point<f64, Logical> =
            Point::from((f64::from(rect.x + local.x), f64::from(rect.y + local.y)));
        self.set_pointer_target(Some(window.clone()));
        self.send_motion(Some((surface, origin)), location, time_ms)?;
        Ok(PointerDelivery::Moved {
            window: window.clone(),
            local,
        })
    }

    /// ボタンを押す / 離す。宛先は直前の [`Self::aim_at`] が決めた surface。
    pub fn press_button(
        &mut self,
        button: PointerButton,
        state: ButtonState,
        time_ms: u32,
    ) -> Result<()> {
        let pointer = self.pointer_handle()?;
        let serial = self.next_serial();
        pointer.button(
            self,
            &ButtonEvent {
                serial,
                time: time_ms,
                button: evdev_button_code(button),
                state: to_smithay_state(state),
            },
        );
        pointer.frame(self);
        Ok(())
    }

    fn pointer_handle(&self) -> Result<smithay::input::pointer::PointerHandle<Self>> {
        self.seat.get_pointer().ok_or_else(|| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "schorl's seat has no pointer",
                self.journal.trace_id().clone(),
            )
        })
    }

    fn send_motion(
        &mut self,
        focus: Option<(
            smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
            Point<f64, Logical>,
        )>,
        location: Point<f64, Logical>,
        time_ms: u32,
    ) -> Result<()> {
        let pointer = self.pointer_handle()?;
        let serial = self.next_serial();
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location,
                serial,
                time: time_ms,
            },
        );
        pointer.frame(self);
        Ok(())
    }
}

impl PointerSink for SchorlCompositor {
    fn deliver(&mut self, event: &PointerEvent) -> Result<Delivery> {
        // `pin code.idempotency.write: require write.idempotency = required`。
        // 同じ鍵で二度来たら、クライアントへは一度しか出さない。
        if !self.delivered.admit(event.idempotency_key.as_str()) {
            return Ok(Delivery::DuplicateIgnored);
        }

        let time_ms = u32::try_from(event.at.millis_since_epoch().rem_euclid(1 << 32)).unwrap_or(0);

        match event.kind {
            PointerEventKind::MotionAbsolute { x_px, y_px } => {
                let window = self.pointer_target.clone().ok_or_else(|| {
                    Error::new(
                        ErrorCode::InvalidArgument,
                        "a pointer motion arrived before any window was aimed at",
                        self.journal.trace_id().clone(),
                    )
                })?;
                self.point_at_local(&window, PixelPoint { x: x_px, y: y_px }, time_ms)?;
            }
            PointerEventKind::Button { button, state } => {
                self.press_button(button, state, time_ms)?;
            }
        }
        Ok(Delivery::Delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_button_numbers_are_the_ones_the_kernel_header_defines() {
        assert_eq!(evdev_button_code(PointerButton::Left), 0x110);
        assert_eq!(evdev_button_code(PointerButton::Right), 0x111);
        assert_eq!(evdev_button_code(PointerButton::Middle), 0x112);
    }
}
