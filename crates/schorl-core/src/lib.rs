//! `schorl-core` — 全 crate が敷く横断基盤。
//!
//! この crate には schorl 固有の振る舞い (板・捕捉・入力・XR) を一切置かない。
//! ここに置くのは `house_style@4.0` が import する `prohibitions@1.0` の
//! 「生成されるコードの掟」だけで、どの domain でも同じ形になるもの。
//!
//! 満たす pin:
//! - `code.error.envelope` / `code.error.http_shape` / `code.error.no_raw_stack` → [`error`]
//! - `code.log.format` / `code.log.required_fields` / `code.log.no_secrets` → [`log`]
//! - `code.secret.source` / `code.secret.no_commit` → [`secret`]
//! - `code.idempotency.write` → [`id::IdempotencyKey`]
//! - `code.retry.max` / `code.retry.backoff` → [`retry`]
//! - `code.time.tz` → [`time`]
//! - `code.id.scheme` → [`id`]
//!
//! 周囲効果 (時刻・乱数・ID 生成・ログ出力・秘密の取得) は
//! `house.effect_boundary.*` に従い、最小の capability trait として切り出してある。
//! 神 capability は作らない。組み立ては境界 (`schorl` bin) で行う。

pub mod error;
pub mod id;
pub mod json;
pub mod log;
pub mod retry;
pub mod secret;
pub mod testing;
pub mod time;

pub use error::{Error, ErrorCode, ErrorEnvelope, Result};
