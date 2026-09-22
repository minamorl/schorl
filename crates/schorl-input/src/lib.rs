//! `schorl-input` — 板の上の操作を Linux へ戻す。
//!
//! 満たす pin:
//! - `v1.cursor_moves: require schorl.v1.cursor.on_panel = moves` —
//!   [`PointerEventKind::MotionAbsolute`] が板の画素位置をそのまま運ぶ。
//! - `v1.click_delivered: require schorl.v1.click = delivered_to_linux` —
//!   [`PointerSink`] が届けた結果を [`Delivery`] で返す。
//! - `v1.key_delivered: require schorl.v1.key_input = delivered_to_linux` —
//!   [`KeyboardSink`] も同じ形。
//! - `code.idempotency.write: require write.idempotency = required` —
//!   どの事象も [`IdempotencyKey`] を持ち、同じ鍵の再送は
//!   [`Delivery::DuplicateIgnored`] になる。二重クリックが起きない。
//! - `house.effect_boundary.no_god_capability` — ポインタとキーボードは別の trait。
//!
//! どの protocol で戻すか (`free schorl.input.pointer_protocol` /
//! `free schorl.input.keyboard_protocol`) はここでは決めない。
//! **実装はこの phase では書かない。** 穴は trait の署名として残す。

use schorl_core::error::Result;
use schorl_core::id::IdempotencyKey;
use schorl_core::time::UtcTimestamp;
use schorl_display::OutputId;

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
    /// 出力上の絶対位置へ動かす。
    ///
    /// 出力を名指しするので、板の画素座標からホスト全体の座標へ換算しなくてよい。
    MotionAbsolute {
        /// 動かす先の出力。
        output: OutputId,
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

/// Linux へ戻すポインタ事象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerEvent {
    /// 重複を防ぐ鍵。
    pub idempotency_key: IdempotencyKey,
    /// 事象が起きた時刻 (UTC)。
    pub at: UtcTimestamp,
    /// 中身。
    pub kind: PointerEventKind,
}

/// Linux へ戻すキー事象。
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

/// ポインタを Linux へ戻す capability。
///
/// **実装は後続 phase。**
pub trait PointerSink: Send {
    /// 一つ届ける。同じ鍵の再送は [`Delivery::DuplicateIgnored`] を返すこと。
    fn deliver(&mut self, event: &PointerEvent) -> Result<Delivery>;
}

/// キー入力を Linux へ戻す capability。
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
                output: OutputId::new("SCHORL-1").expect("valid name"),
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

    #[test]
    fn motion_names_the_output_it_targets() {
        let event = motion("motion-2");
        match event.kind {
            PointerEventKind::MotionAbsolute { output, x_px, y_px } => {
                assert_eq!(output.as_str(), "SCHORL-1");
                assert_eq!((x_px, y_px), (960, 540));
            }
            PointerEventKind::Button { .. } => unreachable!("constructed as motion"),
        }
    }
}
