//! `schorl-input` — ウィンドウの上の操作を、そのウィンドウのクライアントへ届ける。
//!
//! 満たす pin (spec 0.2 の語):
//! - `v1.cursor_moves: require schorl.v1.cursor.on_window = moves` —
//!   [`PointerEventKind::MotionAbsolute`] がウィンドウの画素位置をそのまま運ぶ。
//! - `v1.click_delivered: require schorl.v1.click = delivered_to_window_client` —
//!   [`PointerSink`] が届けた結果を [`Delivery`] で返す。
//! - `v1.key_delivered: require schorl.v1.key_input = delivered_to_window_client` —
//!   [`KeyboardSink`] も同じ形。
//! - `wm.input_ownership: require schorl.input.seat = owned_by_schorl` —
//!   宛先は schorl 自身のクライアントなので、ホストの出力を名指しする欄は持たない。
//! - `code.idempotency.write: require write.idempotency = required` —
//!   どの事象も [`IdempotencyKey`] を持ち、同じ鍵の再送は
//!   [`Delivery::DuplicateIgnored`] になる。二重クリックが起きない。
//! - `house.effect_boundary.no_god_capability` — ポインタとキーボードは別の trait。
//!
//! どう届けるか (`free schorl.compositor.shell_protocols` / `free schorl.window.focus_policy`)
//! はここでは決めない。**実装はこの phase では書かない。** 穴は trait の署名として残す。
//! 0.2 以前にあった `free schorl.input.pointer_protocol` /
//! `free schorl.input.keyboard_protocol` (ホストへ注入する protocol の選択) は、
//! 原文3 でその経路ごと退役した。

use schorl_core::error::Result;
use schorl_core::id::IdempotencyKey;
use schorl_core::time::UtcTimestamp;

/// 届けた結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Delivery {
    /// 届いた。
    Delivered,
    /// 同じ鍵を既に届けていたので何もしなかった。
    DuplicateIgnored,
}

/// ポインタのボタン。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButton {
    /// 左。
    Left,
    /// 右。
    Right,
    /// 中。
    Middle,
}

/// 押下の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ButtonState {
    /// 押した。
    Pressed,
    /// 離した。
    Released,
}

/// キーの物理位置を表す番号。綴りの解釈は Linux 側の配列に委ねる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Keycode(pub u32);

/// ポインタに起きたこと。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerEventKind {
    /// ウィンドウ上の絶対位置へ動かす。
    ///
    /// 座標は宛先ウィンドウの表面ローカル。schorl が seat を持つので、ホストの
    /// 出力を名指しして換算する必要が無い (`wm.input_ownership`)。0.2 以前は
    /// ここに `output: schorl_display::OutputId` があった — 注入経路の語彙であり、
    /// 原文3 で退役した。
    MotionAbsolute {
        /// 左からの画素。
        x_px: i32,
        /// 上からの画素。
        y_px: i32,
    },
    /// ボタンを押す / 離す。
    Button {
        /// どのボタンか。
        button: PointerButton,
        /// 押したか離したか。
        state: ButtonState,
    },
}

/// クライアントへ届けるポインタ事象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerEvent {
    /// 重複を防ぐ鍵。
    pub idempotency_key: IdempotencyKey,
    /// 事象が起きた時刻 (UTC)。
    pub at: UtcTimestamp,
    /// 中身。
    pub kind: PointerEventKind,
}

/// クライアントへ届けるキー事象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// 重複を防ぐ鍵。
    pub idempotency_key: IdempotencyKey,
    /// 事象が起きた時刻 (UTC)。
    pub at: UtcTimestamp,
    /// どのキーか。
    pub keycode: Keycode,
    /// 押したか離したか。
    pub state: ButtonState,
}

/// ポインタをクライアントへ届ける capability。
///
/// **実装は後続 phase。**
pub trait PointerSink: Send {
    /// 一つ届ける。同じ鍵の再送は [`Delivery::DuplicateIgnored`] を返すこと。
    fn deliver(&mut self, event: &PointerEvent) -> Result<Delivery>;
}

/// キー入力をクライアントへ届ける capability。
///
/// **実装は後続 phase。**
pub trait KeyboardSink: Send {
    /// 一つ届ける。同じ鍵の再送は [`Delivery::DuplicateIgnored`] を返すこと。
    fn deliver(&mut self, event: &KeyEvent) -> Result<Delivery>;
}

/// 試験用の差し替え実装 (`house.effect_boundary.substitution`)。
pub mod testing {
    use super::*;
    use std::collections::HashSet;

    /// 届いた物を溜め、鍵で重複を落とす贋物。
    #[derive(Debug, Default)]
    pub struct RecordingSink {
        seen: HashSet<String>,
        pointer_events: Vec<PointerEvent>,
        key_events: Vec<KeyEvent>,
    }

    impl RecordingSink {
        /// 空で作る。
        pub fn new() -> Self {
            Self::default()
        }

        /// 届いたポインタ事象。
        pub fn pointer_events(&self) -> &[PointerEvent] {
            &self.pointer_events
        }

        /// 届いたキー事象。
        pub fn key_events(&self) -> &[KeyEvent] {
            &self.key_events
        }

        fn admit(&mut self, key: &IdempotencyKey) -> Delivery {
            if self.seen.insert(key.as_str().to_owned()) {
                Delivery::Delivered
            } else {
                Delivery::DuplicateIgnored
            }
        }
    }

    impl PointerSink for RecordingSink {
        fn deliver(&mut self, event: &PointerEvent) -> Result<Delivery> {
            let delivery = self.admit(&event.idempotency_key);
            if delivery == Delivery::Delivered {
                self.pointer_events.push(event.clone());
            }
            Ok(delivery)
        }
    }

    impl KeyboardSink for RecordingSink {
        fn deliver(&mut self, event: &KeyEvent) -> Result<Delivery> {
            let delivery = self.admit(&event.idempotency_key);
            if delivery == Delivery::Delivered {
                self.key_events.push(event.clone());
            }
            Ok(delivery)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::RecordingSink;
    use super::*;
    use schorl_core::id::{Id, IdScheme};

    fn key(text: &str) -> IdempotencyKey {
        IdempotencyKey::new(Id::new(IdScheme::Ulid, text).expect("valid text"))
    }

    fn at(millis: i64) -> UtcTimestamp {
        UtcTimestamp::from_millis_since_epoch(millis)
    }

    fn motion(k: &str) -> PointerEvent {
        PointerEvent {
            idempotency_key: key(k),
            at: at(1),
            kind: PointerEventKind::MotionAbsolute {
                x_px: 960,
                y_px: 540,
            },
        }
    }

    #[test]
    fn a_click_reaches_the_sink() {
        let mut sink = RecordingSink::new();
        let click = PointerEvent {
            idempotency_key: key("click-1"),
            at: at(2),
            kind: PointerEventKind::Button {
                button: PointerButton::Left,
                state: ButtonState::Pressed,
            },
        };
        assert_eq!(
            PointerSink::deliver(&mut sink, &click).expect("delivered"),
            Delivery::Delivered
        );
        assert_eq!(sink.pointer_events().len(), 1);
    }

    #[test]
    fn a_key_press_reaches_the_sink() {
        let mut sink = RecordingSink::new();
        let event = KeyEvent {
            idempotency_key: key("key-1"),
            at: at(3),
            keycode: Keycode(30),
            state: ButtonState::Pressed,
        };
        assert_eq!(
            KeyboardSink::deliver(&mut sink, &event).expect("delivered"),
            Delivery::Delivered
        );
        assert_eq!(sink.key_events().len(), 1);
        assert_eq!(sink.key_events()[0].keycode, Keycode(30));
    }

    #[test]
    fn resending_the_same_key_changes_nothing() {
        let mut sink = RecordingSink::new();
        let event = motion("motion-1");
        assert_eq!(
            PointerSink::deliver(&mut sink, &event).expect("delivered"),
            Delivery::Delivered
        );
        assert_eq!(
            PointerSink::deliver(&mut sink, &event).expect("second attempt"),
            Delivery::DuplicateIgnored
        );
        assert_eq!(sink.pointer_events().len(), 1);
    }

    /// 0.2 以前はここで `output` 欄が "SCHORL-1" を名指ししていることを見ていた。
    /// 原文3 でホスト出力を名指しする経路が退役し、欄ごと消えたので、残った不変条件
    /// (ウィンドウ表面ローカルの画素位置をそのまま運ぶこと) だけを見る。
    #[test]
    fn motion_carries_the_surface_local_pixel_position() {
        let event = motion("motion-2");
        match event.kind {
            PointerEventKind::MotionAbsolute { x_px, y_px } => {
                assert_eq!((x_px, y_px), (960, 540));
            }
            PointerEventKind::Button { .. } => unreachable!("constructed as motion"),
        }
    }
}
