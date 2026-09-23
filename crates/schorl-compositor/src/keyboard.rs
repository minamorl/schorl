//! キー入力をクライアントへ配る。
//!
//! `pin v1.key_delivered: require schorl.v1.key_input = delivered_to_window_client`。
//! schorl が seat を持っているので、宿主へ注入して回り込む必要が無い
//! (`pin wm.input_ownership`)。
//!
//! # キー番号の目盛り
//!
//! `wl_keyboard.key` が運ぶのは evdev のキー番号である。xkb は同じキーを
//! `evdev + 8` で数える。smithay は送信時に `key.raw_code().raw() - 8` を書いて
//! いる (smithay 0.7.0 `src/wayland/seat/keyboard.rs`)。xkbcommon 側も
//! `Keycode::new(KEY_A as u32 + 8)` と綴っている (xkbcommon 0.8.0 の
//! `src/xkb/mod.rs` の例)。よってここは evdev 番号を受け取り、xkb へ渡す前に
//! [`EVDEV_TO_XKB_OFFSET`] を足す。

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_input::{ButtonState, Delivery, KeyEvent, KeyboardSink, Keycode};

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{FilterResult, Keycode as XkbKeycode};

use crate::state::SchorlCompositor;
use crate::window::WindowId;

/// evdev の番号と xkb の番号の差。
pub const EVDEV_TO_XKB_OFFSET: u32 = 8;

const fn to_key_state(state: ButtonState) -> KeyState {
    match state {
        ButtonState::Pressed => KeyState::Pressed,
        ButtonState::Released => KeyState::Released,
    }
}

impl SchorlCompositor {
    /// キーボードの宛先を決める。`None` なら誰にも届かなくなる。
    pub fn focus_keyboard_on(&mut self, window: Option<&WindowId>) -> Result<()> {
        let surface = match window {
            None => None,
            Some(id) => Some(
                self.windows
                    .get(id)
                    .ok_or_else(|| {
                        Error::new(
                            ErrorCode::InvalidArgument,
                            "no such window is being tracked",
                            self.journal.trace_id().clone(),
                        )
                        .with_detail("window", id.as_str())
                    })?
                    .handle()
                    .wl_surface()
                    .clone(),
            ),
        };

        let keyboard = self.keyboard_handle()?;
        let serial = self.next_serial();
        keyboard.set_focus(self, surface, serial);
        Ok(())
    }

    /// キーを一つ配る。`keycode` は evdev の番号。
    pub fn press_key(&mut self, keycode: Keycode, state: ButtonState, time_ms: u32) -> Result<()> {
        let keyboard = self.keyboard_handle()?;
        let serial = self.next_serial();
        let xkb = XkbKeycode::new(keycode.0.saturating_add(EVDEV_TO_XKB_OFFSET));
        // schorl はまだどのキーも自分で食べない。全部クライアントへ通す。
        keyboard.input::<(), _>(
            self,
            xkb,
            to_key_state(state),
            serial,
            time_ms,
            |_, _, _| FilterResult::Forward,
        );
        Ok(())
    }

    fn keyboard_handle(&self) -> Result<smithay::input::keyboard::KeyboardHandle<Self>> {
        self.seat.get_keyboard().ok_or_else(|| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "schorl's seat has no keyboard",
                self.journal.trace_id().clone(),
            )
        })
    }
}

impl KeyboardSink for SchorlCompositor {
    fn deliver(&mut self, event: &KeyEvent) -> Result<Delivery> {
        if !self.delivered.admit(event.idempotency_key.as_str()) {
            return Ok(Delivery::DuplicateIgnored);
        }
        let time_ms = u32::try_from(event.at.millis_since_epoch().rem_euclid(1 << 32)).unwrap_or(0);
        self.press_key(event.keycode, event.state, time_ms)?;
        Ok(Delivery::Delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offset_between_evdev_and_xkb_is_eight() {
        // smithay は送信時に 8 を引き、xkbcommon の例は 8 を足している。
        assert_eq!(EVDEV_TO_XKB_OFFSET, 8);
        let a = XkbKeycode::new(30 + EVDEV_TO_XKB_OFFSET);
        assert_eq!(a.raw(), 38);
    }
}
