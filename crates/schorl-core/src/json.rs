//! 最小の JSON 直列化。
//!
//! `pin code.log.format: require log.format = json` を満たすために要る。外部 crate を
//! 足していないのは、この phase で依存の一次資料を読む経路を持っていないため
//! (`P-EX-2` は記憶した API の形から書くことを禁じている)。書けるのは値の生成だけで、
//! 解析器は持たない。

use core::fmt::Write as _;

/// JSON の値。ログ行とエラー封筒だけを組むための最小の枝。
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    /// `null`。
    Null,
    /// 真偽値。
    Bool(bool),
    /// 64bit 符号付き整数。浮動小数は封筒にもログにも出さないので持たない。
    Int(i64),
    /// 文字列。出力時に [`escape_into`] で逃がす。
    Text(String),
    /// 配列。
    Array(Vec<JsonValue>),
    /// キーの順序を保つオブジェクト。封筒の鍵の順序を固定したいので `Vec` で持つ。
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    /// 文字列の枝を作る補助。
    pub fn text(value: impl Into<String>) -> Self {
        JsonValue::Text(value.into())
    }

    /// JSON テキストへ描画する。
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.render_into(&mut out);
        out
    }

    /// 既存のバッファへ描画する。
    pub fn render_into(&self, out: &mut String) {
        match self {
            JsonValue::Null => out.push_str("null"),
            JsonValue::Bool(true) => out.push_str("true"),
            JsonValue::Bool(false) => out.push_str("false"),
            JsonValue::Int(n) => {
                // `String` への `write!` は失敗しない。握り潰しであることを明示する。
                let _ = write!(out, "{n}");
            }
            JsonValue::Text(s) => escape_into(s, out),
            JsonValue::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.render_into(out);
                }
                out.push(']');
            }
            JsonValue::Object(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    escape_into(key, out);
                    out.push(':');
                    value.render_into(out);
                }
                out.push('}');
            }
        }
    }
}

/// 文字列を JSON の引用符つき literal として `out` へ書き出す。
pub fn escape_into(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_object_in_declared_key_order() {
        let value = JsonValue::Object(vec![
            ("b".to_owned(), JsonValue::Int(2)),
            ("a".to_owned(), JsonValue::text("x")),
        ]);
        assert_eq!(value.render(), r#"{"b":2,"a":"x"}"#);
    }

    #[test]
    fn escapes_quotes_backslashes_newlines_and_control_characters() {
        let value = JsonValue::text("a\"b\\c\nd\u{1}");
        assert_eq!(value.render(), "\"a\\\"b\\\\c\\nd\\u0001\"");
    }

    #[test]
    fn renders_scalars() {
        assert_eq!(JsonValue::Null.render(), "null");
        assert_eq!(JsonValue::Bool(true).render(), "true");
        assert_eq!(JsonValue::Int(-7).render(), "-7");
    }
}
