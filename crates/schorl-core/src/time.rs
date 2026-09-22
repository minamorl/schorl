//! 時刻。
//!
//! `pin code.time.tz: require time.storage = utc`。型が UTC 以外を表せないようにして
//! 満たす。ローカル時刻を持つ型はこの workspace に置かない。
//!
//! 現在時刻の取得は周囲効果なので [`Clock`] という最小の capability にしてある
//! (`house.effect_boundary.form` / `.location`)。

use std::time::{SystemTime, UNIX_EPOCH};

/// UTC の瞬間。Unix epoch からのミリ秒でだけ保持する。
///
/// 暦も timezone も持たない。文字列化は epoch ミリ秒の整数で、地域設定に依らない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcTimestamp {
    millis: i64,
}

impl UtcTimestamp {
    /// epoch からのミリ秒から作る。負は epoch 以前。
    pub const fn from_millis_since_epoch(millis: i64) -> Self {
        Self { millis }
    }

    /// epoch からのミリ秒。
    pub const fn millis_since_epoch(self) -> i64 {
        self.millis
    }
}

/// 現在時刻を得る capability。
///
/// 実装を差し替えられることが `house.effect_boundary.substitution` の要求。
/// 試験用の固定実装は [`crate::testing::FixedClock`]。
pub trait Clock: Send + Sync {
    /// いまの UTC。
    fn now_utc(&self) -> UtcTimestamp;
}

/// ホストの単調でない壁時計を読む adapter。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> UtcTimestamp {
        // panic しない (`rust.error_boundary.no_panic`)。epoch 以前は負値で表す。
        let millis = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            Err(e) => i64::try_from(e.duration().as_millis())
                .map(|m| -m)
                .unwrap_or(i64::MIN),
        };
        UtcTimestamp::from_millis_since_epoch(millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_trips_through_epoch_millis() {
        let t = UtcTimestamp::from_millis_since_epoch(1_700_000_000_123);
        assert_eq!(t.millis_since_epoch(), 1_700_000_000_123);
    }

    #[test]
    fn system_clock_is_after_2020() {
        let t = SystemClock.now_utc();
        assert!(t.millis_since_epoch() > 1_577_836_800_000);
    }
}
