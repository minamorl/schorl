//! 眠る口。
//!
//! 一次資料が眠りを要求している。`XR_MND_headless` の逐語:
//!
//! > Because flink:xrWaitFrame is not required, an application using a headless
//! > session should: sleep periodically to avoid consuming all available system
//! > resources in a busy-wait loop.
//!
//! (OpenXR-Docs `specification/sources/chapters/extensions/mnd/mnd_headless.adoc`,
//! Last Modified Date 2025-08-20)
//!
//! 眠りは周囲効果なので、実装を差し替えられる最小の capability にしてある
//! (`house.effect_boundary.form` / `house.effect_boundary.substitution`)。
//! 試験は [`testing::CountingSleeper`] に差し替えるので実時間を使わない。

use std::time::Duration;

/// しばらく眠る capability。
pub trait Sleeper: Send + Sync {
    /// `millis` ミリ秒ほど眠る。0 は「眠らない」を意味してよい。
    fn sleep_millis(&self, millis: u64);
}

/// 走っているスレッドを実際に眠らせる adapter。
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep_millis(&self, millis: u64) {
        if millis > 0 {
            std::thread::sleep(Duration::from_millis(millis));
        }
    }
}

/// 試験用の差し替え実装 (`house.effect_boundary.substitution`)。
pub mod testing {
    use super::Sleeper;
    use std::sync::Mutex;

    /// 眠らずに、頼まれた長さを溜めるだけの贋物。
    #[derive(Debug, Default)]
    pub struct CountingSleeper {
        requested: Mutex<Vec<u64>>,
    }

    impl CountingSleeper {
        /// 空で作る。
        pub fn new() -> Self {
            Self::default()
        }

        /// 頼まれた長さの並び。
        pub fn requested(&self) -> Vec<u64> {
            match self.requested.lock() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            }
        }
    }

    impl Sleeper for CountingSleeper {
        fn sleep_millis(&self, millis: u64) {
            match self.requested.lock() {
                Ok(mut g) => g.push(millis),
                Err(p) => p.into_inner().push(millis),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::CountingSleeper;
    use super::*;

    #[test]
    fn a_counting_sleeper_records_without_waiting() {
        let sleeper = CountingSleeper::new();
        sleeper.sleep_millis(16);
        sleeper.sleep_millis(0);
        assert_eq!(sleeper.requested(), vec![16, 0]);
    }

    #[test]
    fn a_zero_length_sleep_is_a_no_op() {
        // 実時間を使わない枝だけを踏む。
        ThreadSleeper.sleep_millis(0);
    }
}
