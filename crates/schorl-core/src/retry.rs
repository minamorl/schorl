//! 再試行。
//!
//! 満たす pin:
//! - `code.retry.max: range retry.max = 0..5`
//! - `code.retry.backoff: require retry.backoff = exponential_jitter`
//!
//! [`Backoff`] は指数 + ジッタの枝しか持たないので、定数待ちや線形待ちは表現できない。
//! 乱数は周囲効果なので、ジッタの一様値 `[0,1)` は引数で受ける
//! (`house.effect_boundary.form` — 乱数源を握り込まない)。

use crate::error::{Error, ErrorCode, Result};
use crate::id::TraceId;

/// `retry.max` の上限。pin の範囲 `0..5` の端。
pub const MAX_RETRIES_LIMIT: u8 = 5;

/// 待ち方。指数 + ジッタのみ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// 指数増加に full jitter を掛ける。
    ExponentialJitter {
        /// 初回の基準待ち時間 (ミリ秒)。
        base_millis: u64,
        /// 指数増加の頭打ち (ミリ秒)。
        cap_millis: u64,
    },
}

/// 再試行の方針。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_retries: u8,
    backoff: Backoff,
}

impl RetryPolicy {
    /// 方針を作る。`max_retries` が pin の範囲外なら封筒で拒む。
    pub fn new(max_retries: u8, backoff: Backoff) -> Result<Self> {
        if max_retries > MAX_RETRIES_LIMIT {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "retry.max is pinned to the range 0..5",
                TraceId::unattributed(),
            )
            .with_detail("requested", i64::from(max_retries))
            .with_detail("limit", i64::from(MAX_RETRIES_LIMIT)));
        }
        Ok(Self {
            max_retries,
            backoff,
        })
    }

    /// 再試行の上限回数。
    pub const fn max_retries(self) -> u8 {
        self.max_retries
    }

    /// 待ち方。
    pub const fn backoff(self) -> Backoff {
        self.backoff
    }

    /// `attempt` 回目 (0 始まり) の待ち時間。
    ///
    /// `jitter_unit` は `[0,1)` の一様値。範囲外は端に丸める。呼ぶ側が乱数源を持つ。
    pub fn delay_millis(self, attempt: u32, jitter_unit: f64) -> u64 {
        let Backoff::ExponentialJitter {
            base_millis,
            cap_millis,
        } = self.backoff;
        let exponential = base_millis.saturating_mul(1_u64 << attempt.min(32));
        let ceiling = exponential.min(cap_millis);
        let unit = if jitter_unit.is_nan() {
            0.0
        } else {
            jitter_unit.clamp(0.0, 1.0)
        };
        // full jitter: [0, ceiling] の一様。
        (ceiling as f64 * unit) as u64
    }

    /// 上限に達したか。
    pub const fn should_retry(self, attempts_made: u8) -> bool {
        attempts_made < self.max_retries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max: u8) -> Result<RetryPolicy> {
        RetryPolicy::new(
            max,
            Backoff::ExponentialJitter {
                base_millis: 100,
                cap_millis: 2_000,
            },
        )
    }

    #[test]
    fn rejects_more_than_five_retries() {
        let err = policy(6).expect_err("6 is outside the pinned range");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn accepts_the_whole_pinned_range() {
        for max in 0..=MAX_RETRIES_LIMIT {
            assert!(policy(max).is_ok(), "max={max} is inside 0..5");
        }
    }

    #[test]
    fn delay_grows_exponentially_and_is_capped() {
        let p = policy(5).expect("valid policy");
        assert_eq!(p.delay_millis(0, 1.0), 100);
        assert_eq!(p.delay_millis(1, 1.0), 200);
        assert_eq!(p.delay_millis(2, 1.0), 400);
        assert_eq!(p.delay_millis(10, 1.0), 2_000);
    }

    #[test]
    fn jitter_spans_zero_to_the_ceiling() {
        let p = policy(5).expect("valid policy");
        assert_eq!(p.delay_millis(3, 0.0), 0);
        assert_eq!(p.delay_millis(3, 0.5), 400);
        assert_eq!(p.delay_millis(3, 1.0), 800);
        assert_eq!(p.delay_millis(3, -1.0), 0, "out of range clamps low");
        assert_eq!(p.delay_millis(3, 9.0), 800, "out of range clamps high");
    }

    #[test]
    fn stops_retrying_at_the_limit() {
        let p = policy(2).expect("valid policy");
        assert!(p.should_retry(0));
        assert!(p.should_retry(1));
        assert!(!p.should_retry(2));
    }
}
