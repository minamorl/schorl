//! 宿主 compositor から PC の keyboard / mouse を受け取る境界。
//!
//! evdev や libinput を直接開かず、宿主上の小さな winit window が focus を持つ間だけ
//! 入力を受ける。したがって普通の desktop へ戻る操作は、その window から focus を
//! 外すだけでよい。将来 tablet source を足す場合も、この module と並ぶ別 source として
//! [`HostInputEvent`] を作ればよく、compositor の seat 配信は共有できる。

use std::time::Duration;

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_input::{ButtonState, Keycode, PointerButton};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{Window, WindowId};

/// 宿主の入力窓から一巡ぶんに届いた出来事。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostInputEvent {
    /// 入力窓の中の位置。各軸は `0..=1`。
    Motion {
        /// 左端 0、右端 1。
        x: f64,
        /// 上端 0、下端 1。
        y: f64,
    },
    /// mouse button。
    Button {
        /// どの button か。
        button: PointerButton,
        /// 押下か解放か。
        state: ButtonState,
    },
    /// wheel / touchpad の scroll 量。正は右 / 下。
    Scroll {
        /// 水平方向の量。
        horizontal: f64,
        /// 垂直方向の量。
        vertical: f64,
    },
    /// Linux evdev keycode。
    Key {
        /// Linux evdev keycode。
        keycode: Keycode,
        /// 押下か解放か。
        state: ButtonState,
    },
    /// F8。VR 内で操作する window を次へ送る。
    SelectNextWindow,
    /// 入力窓を閉じた。XR session も普通に畳む。
    CloseRequested,
}

/// focus 可能な宿主側の入力窓。
pub struct HostInput {
    event_loop: EventLoop<()>,
    app: InputApplication,
}

impl core::fmt::Debug for HostInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostInput")
            .field("window_created", &self.app.window.is_some())
            .finish_non_exhaustive()
    }
}

impl HostInput {
    /// 宿主 display へ接続する。window 自体は最初の [`Self::poll`] で作られる。
    pub fn open() -> Result<Self> {
        let event_loop = EventLoop::new().map_err(|e| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "could not connect the PC input window to the host compositor",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })?;
        Ok(Self {
            event_loop,
            app: InputApplication::default(),
        })
    }

    /// 待たずに宿主の出来事を吸う。
    pub fn poll(&mut self) -> Result<Vec<HostInputEvent>> {
        let status = self
            .event_loop
            .pump_app_events(Some(Duration::ZERO), &mut self.app);
        if let Some(reason) = self.app.creation_failure.take() {
            return Err(Error::new(
                ErrorCode::CapabilityUnavailable,
                "could not create the PC input window",
                TraceId::unattributed(),
            )
            .with_detail("reason", reason));
        }
        if matches!(status, PumpStatus::Exit(_))
            && !self
                .app
                .events
                .iter()
                .any(|event| matches!(event, HostInputEvent::CloseRequested))
        {
            self.app.events.push(HostInputEvent::CloseRequested);
        }
        Ok(std::mem::take(&mut self.app.events))
    }
}

#[derive(Debug, Default)]
struct InputApplication {
    window: Option<Window>,
    events: Vec<HostInputEvent>,
    creation_failure: Option<String>,
}

impl ApplicationHandler for InputApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.creation_failure.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("schorl input — focus this window to control VR (F8: next window)")
            .with_inner_size(LogicalSize::new(960.0, 540.0));
        match event_loop.create_window(attributes) {
            Ok(window) => self.window = Some(window),
            Err(e) => {
                self.creation_failure = Some(e.to_string());
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                self.events.push(HostInputEvent::CloseRequested);
                event_loop.exit();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let size = window.inner_size();
                if size.width > 0 && size.height > 0 {
                    self.events.push(HostInputEvent::Motion {
                        x: (position.x / f64::from(size.width)).clamp(0.0, 1.0),
                        y: (position.y / f64::from(size.height)).clamp(0.0, 1.0),
                    });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => Some(PointerButton::Left),
                    MouseButton::Right => Some(PointerButton::Right),
                    MouseButton::Middle => Some(PointerButton::Middle),
                    _ => None,
                };
                if let Some(button) = button {
                    self.events.push(HostInputEvent::Button {
                        button,
                        state: button_state(state),
                    });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (horizontal, vertical) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        (f64::from(x) * 15.0, f64::from(y) * -15.0)
                    }
                    MouseScrollDelta::PixelDelta(point) => (point.x, -point.y),
                };
                self.events.push(HostInputEvent::Scroll {
                    horizontal,
                    vertical,
                });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let Some(scancode) = event.physical_key.to_scancode() else {
                    return;
                };
                let state = button_state(event.state);
                // Linux input-event-codes.h: KEY_F8 = 66。release も食べ、client に
                // 片側だけの key event を残さない。
                if scancode == 66 {
                    if state == ButtonState::Pressed && !event.repeat {
                        self.events.push(HostInputEvent::SelectNextWindow);
                    }
                } else {
                    self.events.push(HostInputEvent::Key {
                        keycode: Keycode(scancode),
                        state,
                    });
                }
            }
            _ => {}
        }
    }
}

const fn button_state(state: ElementState) -> ButtonState {
    match state {
        ElementState::Pressed => ButtonState::Pressed,
        ElementState::Released => ButtonState::Released,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::KeyCode;

    #[test]
    fn host_element_states_keep_press_and_release_distinct() {
        assert_eq!(button_state(ElementState::Pressed), ButtonState::Pressed);
        assert_eq!(button_state(ElementState::Released), ButtonState::Released);
    }

    #[test]
    fn winit_linux_scancodes_are_the_evdev_numbers_the_seat_expects() {
        assert_eq!(KeyCode::KeyA.to_scancode(), Some(30));
        assert_eq!(KeyCode::F8.to_scancode(), Some(66));
    }
}
