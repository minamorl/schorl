//! `schorl-verify` — 緑と受け入れを混ぜない。
//!
//! 満たす pin:
//! - `verify.machine_scope: require schorl.machine_verifiable = { build_passes,
//!   openxr_session_opens, client_frame_reaches_swapchain,
//!   input_event_delivered_to_client, unit_tests }` (spec 0.2) →
//!   [`MachineCheck`] はこの五つだけを枝に持つ。0.1 の
//!   `capture_returns_real_frame` / `input_injection_delivered` は、原文3 で
//!   capture 経路と注入経路が消えたため、同じ不変条件 (画素が実際に届く /
//!   入力が実際に届く) を経路非依存の語へ改鍵したもの。項目数は 5 のままで、
//!   spec に無い check をここで発明していない。
//! - `verify.hmd_gate: require schorl.hmd_acceptance = human_wearing_quest_3` →
//!   [`accept_by_human`] は Quest 3 を被った人の証言でしか通らない。
//! - `verify.no_green_substitute: forbid schorl.machine_green =
//!   substitute_for_hmd_acceptance` → 機械の結果から受け入れを作る関数は
//!   [`hmd_acceptance_from_machine`] だけで、これは常に `Unknown` を返す。
//! - `property verify.green_is_not_acceptance: forall run . machine_green(run) == true
//!   => hmd_accepted(run) == unknown` → 下の試験が全 243 通りを総当たりで押さえる。

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_core::time::UtcTimestamp;
use schorl_scope::Hmd;

/// HMD 無しで機械が確かめられること。この五つがすべて。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MachineCheck {
    /// ビルドが通ること。
    BuildPasses,
    /// OpenXR ランタイムに対してセッションが開けること。
    OpenxrSessionOpens,
    /// クライアントの一枚が swapchain まで届くこと。
    ClientFrameReachesSwapchain,
    /// 入力事象がクライアントまで届くこと。
    InputEventDeliveredToClient,
    /// 単体試験。
    UnitTests,
}

impl MachineCheck {
    /// 五つすべて。
    pub const ALL: [MachineCheck; 5] = [
        MachineCheck::BuildPasses,
        MachineCheck::OpenxrSessionOpens,
        MachineCheck::ClientFrameReachesSwapchain,
        MachineCheck::InputEventDeliveredToClient,
        MachineCheck::UnitTests,
    ];

    /// 報告に出す綴り。
    pub const fn as_str(self) -> &'static str {
        match self {
            MachineCheck::BuildPasses => "build_passes",
            MachineCheck::OpenxrSessionOpens => "openxr_session_opens",
            MachineCheck::ClientFrameReachesSwapchain => "client_frame_reaches_swapchain",
            MachineCheck::InputEventDeliveredToClient => "input_event_delivered_to_client",
            MachineCheck::UnitTests => "unit_tests",
        }
    }
}

/// 一つの検査の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckOutcome {
    /// 走らせていない。
    NotRun,
    /// 落ちた。
    Red,
    /// 通った。
    Green,
}

impl CheckOutcome {
    /// 報告に出す綴り。
    pub const fn as_str(self) -> &'static str {
        match self {
            CheckOutcome::NotRun => "not_run",
            CheckOutcome::Red => "red",
            CheckOutcome::Green => "green",
        }
    }
}

/// 一回分の機械検査。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineRun {
    outcomes: [CheckOutcome; 5],
}

impl MachineRun {
    /// 何も走らせていない状態。
    pub const fn new() -> Self {
        Self {
            outcomes: [CheckOutcome::NotRun; 5],
        }
    }

    const fn index(check: MachineCheck) -> usize {
        match check {
            MachineCheck::BuildPasses => 0,
            MachineCheck::OpenxrSessionOpens => 1,
            MachineCheck::ClientFrameReachesSwapchain => 2,
            MachineCheck::InputEventDeliveredToClient => 3,
            MachineCheck::UnitTests => 4,
        }
    }

    /// 結果を記録する。
    pub const fn record(mut self, check: MachineCheck, outcome: CheckOutcome) -> Self {
        self.outcomes[Self::index(check)] = outcome;
        self
    }

    /// 記録した結果。
    pub const fn outcome(&self, check: MachineCheck) -> CheckOutcome {
        self.outcomes[Self::index(check)]
    }

    /// 五つすべてが緑か。
    pub fn is_all_green(&self) -> bool {
        self.outcomes
            .iter()
            .all(|o| matches!(o, CheckOutcome::Green))
    }

    /// 検査と結果の並び。
    pub fn entries(&self) -> impl Iterator<Item = (MachineCheck, CheckOutcome)> + '_ {
        MachineCheck::ALL.into_iter().map(|c| (c, self.outcome(c)))
    }
}

impl Default for MachineRun {
    fn default() -> Self {
        Self::new()
    }
}

/// 実機での受け入れ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HmdAcceptance {
    /// 分からない。機械がいくら緑でもここから動かない。
    Unknown,
    /// Quest 3 を被った人が受け入れた。
    AcceptedByHumanWearingQuest3 {
        /// 被って確かめた時刻 (UTC)。
        at: UtcTimestamp,
    },
}

/// 実機を被って確かめた人の証言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanWitness {
    /// 被っていた機械。
    pub hmd: Hmd,
    /// 人が実際に被っていたか。
    pub worn_by_human: bool,
    /// 確かめた時刻 (UTC)。
    pub at: UtcTimestamp,
}

/// 機械の結果から受け入れを導く、唯一の関数。
///
/// **常に [`HmdAcceptance::Unknown`] を返す。** 引数を見ないのは仕様であって
/// 手抜きではない。`verify.no_green_substitute` は、緑から受け入れを作ることを
/// 禁じている。
pub const fn hmd_acceptance_from_machine(_run: &MachineRun) -> HmdAcceptance {
    HmdAcceptance::Unknown
}

/// 人の証言から受け入れを作る。
///
/// Quest 3 を被っていなければ通さない (`verify.hmd_gate`)。
pub fn accept_by_human(witness: &HumanWitness) -> Result<HmdAcceptance> {
    if !witness.worn_by_human {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            "hmd acceptance requires a human actually wearing the headset",
            TraceId::unattributed(),
        )
        .with_detail("hmd", witness.hmd.as_str()));
    }
    match witness.hmd {
        Hmd::Quest3 => Ok(HmdAcceptance::AcceptedByHumanWearingQuest3 { at: witness.at }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTCOMES: [CheckOutcome; 3] =
        [CheckOutcome::NotRun, CheckOutcome::Red, CheckOutcome::Green];

    fn all_runs() -> Vec<MachineRun> {
        let mut runs = Vec::new();
        for a in OUTCOMES {
            for b in OUTCOMES {
                for c in OUTCOMES {
                    for d in OUTCOMES {
                        for e in OUTCOMES {
                            runs.push(
                                MachineRun::new()
                                    .record(MachineCheck::BuildPasses, a)
                                    .record(MachineCheck::OpenxrSessionOpens, b)
                                    .record(MachineCheck::ClientFrameReachesSwapchain, c)
                                    .record(MachineCheck::InputEventDeliveredToClient, d)
                                    .record(MachineCheck::UnitTests, e),
                            );
                        }
                    }
                }
            }
        }
        runs
    }

    #[test]
    fn machine_green_never_becomes_acceptance() {
        let runs = all_runs();
        assert_eq!(runs.len(), 243, "3^5 combinations");
        for run in runs {
            assert_eq!(
                hmd_acceptance_from_machine(&run),
                HmdAcceptance::Unknown,
                "even {run:?} leaves acceptance unknown"
            );
        }
    }

    #[test]
    fn an_all_green_run_is_still_unknown() {
        let run = MachineCheck::ALL
            .into_iter()
            .fold(MachineRun::new(), |acc, check| {
                acc.record(check, CheckOutcome::Green)
            });
        assert!(run.is_all_green());
        assert_eq!(hmd_acceptance_from_machine(&run), HmdAcceptance::Unknown);
    }

    #[test]
    fn the_machine_scope_is_exactly_the_five_pinned_checks() {
        let names: Vec<&str> = MachineCheck::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            names,
            [
                "build_passes",
                "openxr_session_opens",
                "client_frame_reaches_swapchain",
                "input_event_delivered_to_client",
                "unit_tests",
            ]
        );
    }

    #[test]
    fn acceptance_needs_a_human_actually_wearing_the_headset() {
        let at = UtcTimestamp::from_millis_since_epoch(1_700_000_000_000);
        let not_worn = HumanWitness {
            hmd: Hmd::Quest3,
            worn_by_human: false,
            at,
        };
        assert_eq!(
            accept_by_human(&not_worn)
                .expect_err("nobody wore it")
                .code(),
            ErrorCode::InvalidArgument
        );

        let worn = HumanWitness {
            hmd: Hmd::Quest3,
            worn_by_human: true,
            at,
        };
        assert_eq!(
            accept_by_human(&worn).expect("accepted"),
            HmdAcceptance::AcceptedByHumanWearingQuest3 { at }
        );
    }

    #[test]
    fn a_run_records_each_check_separately() {
        let run = MachineRun::new()
            .record(MachineCheck::BuildPasses, CheckOutcome::Green)
            .record(MachineCheck::UnitTests, CheckOutcome::Red);
        assert_eq!(run.outcome(MachineCheck::BuildPasses), CheckOutcome::Green);
        assert_eq!(run.outcome(MachineCheck::UnitTests), CheckOutcome::Red);
        assert_eq!(
            run.outcome(MachineCheck::OpenxrSessionOpens),
            CheckOutcome::NotRun
        );
        assert!(!run.is_all_green());
        assert_eq!(run.entries().count(), 5);
    }
}
