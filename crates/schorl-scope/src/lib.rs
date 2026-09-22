//! `schorl-scope` — 何を作り、何を作らないかを型と定数で持つ。
//!
//! 散文で「範囲外」と書くと、後の phase が読み飛ばせる。ここでは範囲外を
//! 実行できる値にして、試験で押さえる。
//!
//! 満たす pin:
//! - `host.platform: require schorl.host.platform = linux` → [`HostPlatform`]
//! - `scope.deliverable: require schorl.deliverable = self_built_vr_work_environment` → [`Deliverable`]
//! - `hmd.model: require schorl.hmd = quest_3` → [`Hmd`]
//! - `space.background: require schorl.virtual_space.background = black` → [`Background`]
//! - `display.presence: require schorl.virtual_space.display = present` → [`DisplayPresence`]
//! - `display.purpose: require schorl.display.purpose = work` → [`DisplayPurpose`]
//! - `v1.panel_count: require schorl.v1.panel.count = 1` → [`V1_PANEL_COUNT`]
//! - `v1.scope_excluded: forbid schorl.v1.scope = { multi_panel, curved_panel,
//!   passthrough, environment_switching, audio }` → [`V1_EXCLUDED_FEATURES`]
//!
//! いずれの列挙も、pin が許した枝しか持たない。たとえば [`Background`] に
//! 黒以外の枝は無いので、背景を選び直すことはコンパイルできない。

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;

/// 動かす土台。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostPlatform {
    /// Linux。pin が許した唯一の枝。
    Linux,
}

/// 作る物。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Deliverable {
    /// 自作の VR 作業環境。上流部品を借りることは禁じられていない。
    SelfBuiltVrWorkEnvironment,
}

/// 被る機械。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hmd {
    /// Quest 3。pin が許した唯一の枝。
    Quest3,
}

impl Hmd {
    /// 報告に出す綴り。
    pub const fn as_str(self) -> &'static str {
        match self {
            Hmd::Quest3 => "quest_3",
        }
    }
}

/// 仮想空間の背景。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Background {
    /// 真っ暗。pin が許した唯一の枝。
    Black,
}

impl Background {
    /// 全画素を埋める線形 RGBA。真っ暗なので不透明の黒。
    pub const fn clear_color_rgba(self) -> [f32; 4] {
        match self {
            Background::Black => [0.0, 0.0, 0.0, 1.0],
        }
    }
}

/// 仮想空間にディスプレイが在ること。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplayPresence {
    /// 在る。
    Present,
}

/// ディスプレイの用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplayPurpose {
    /// 作業。
    Work,
}

/// v1 の版図に関わる機能。
///
/// 前半は v1 が満たす物、後半は `v1.scope_excluded` が禁じた物。
/// 禁じた側も列挙に入れてあるのは、名前を持たないと試験で押さえられないため。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    /// 板が一枚あること。
    SinglePanel,
    /// 板の中身が Linux のディスプレイであること。
    PanelShowsLinuxDisplay,
    /// コントローラで掴んで置き直せること。
    PanelGrabReposition,
    /// 板の上でカーソルが動くこと。
    CursorOnPanel,
    /// クリックが Linux へ届くこと。
    ClickDeliveredToLinux,
    /// キー入力が Linux へ届くこと。
    KeyInputDeliveredToLinux,
    /// 板を複数出すこと。**v1 の範囲外**。
    MultiPanel,
    /// 板を曲げること。**v1 の範囲外**。
    CurvedPanel,
    /// パススルー。**v1 の範囲外**。
    Passthrough,
    /// 環境の切り替え。**v1 の範囲外**。
    EnvironmentSwitching,
    /// 音声。**v1 の範囲外**。
    Audio,
}

impl Feature {
    /// 名前の付いた機能のすべて。
    pub const ALL: [Feature; 11] = [
        Feature::SinglePanel,
        Feature::PanelShowsLinuxDisplay,
        Feature::PanelGrabReposition,
        Feature::CursorOnPanel,
        Feature::ClickDeliveredToLinux,
        Feature::KeyInputDeliveredToLinux,
        Feature::MultiPanel,
        Feature::CurvedPanel,
        Feature::Passthrough,
        Feature::EnvironmentSwitching,
        Feature::Audio,
    ];

    /// 報告に出す綴り。
    pub const fn as_str(self) -> &'static str {
        match self {
            Feature::SinglePanel => "single_panel",
            Feature::PanelShowsLinuxDisplay => "panel_shows_linux_display",
            Feature::PanelGrabReposition => "panel_grab_reposition",
            Feature::CursorOnPanel => "cursor_on_panel",
            Feature::ClickDeliveredToLinux => "click_delivered_to_linux",
            Feature::KeyInputDeliveredToLinux => "key_input_delivered_to_linux",
            Feature::MultiPanel => "multi_panel",
            Feature::CurvedPanel => "curved_panel",
            Feature::Passthrough => "passthrough",
            Feature::EnvironmentSwitching => "environment_switching",
            Feature::Audio => "audio",
        }
    }

    /// v1 で禁じられているか。
    pub const fn excluded_from_v1(self) -> bool {
        matches!(
            self,
            Feature::MultiPanel
                | Feature::CurvedPanel
                | Feature::Passthrough
                | Feature::EnvironmentSwitching
                | Feature::Audio
        )
    }
}

/// v1 の板の枚数。
pub const V1_PANEL_COUNT: u8 = 1;

/// `v1.scope_excluded` が名指しした五つ。
pub const V1_EXCLUDED_FEATURES: [Feature; 5] = [
    Feature::MultiPanel,
    Feature::CurvedPanel,
    Feature::Passthrough,
    Feature::EnvironmentSwitching,
    Feature::Audio,
];

/// v1 が満たす六つ。
pub const V1_INCLUDED_FEATURES: [Feature; 6] = [
    Feature::SinglePanel,
    Feature::PanelShowsLinuxDisplay,
    Feature::PanelGrabReposition,
    Feature::CursorOnPanel,
    Feature::ClickDeliveredToLinux,
    Feature::KeyInputDeliveredToLinux,
];

/// v1 が動く前提の一式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V1Contract {
    /// 土台。
    pub host: HostPlatform,
    /// 作る物。
    pub deliverable: Deliverable,
    /// 被る機械。
    pub hmd: Hmd,
    /// 背景。
    pub background: Background,
    /// ディスプレイが在ること。
    pub display: DisplayPresence,
    /// ディスプレイの用途。
    pub purpose: DisplayPurpose,
    /// 板の枚数。
    pub panel_count: u8,
}

impl V1Contract {
    /// pin どおりの一式。選び直せる欄は無い。
    pub const V1: V1Contract = V1Contract {
        host: HostPlatform::Linux,
        deliverable: Deliverable::SelfBuiltVrWorkEnvironment,
        hmd: Hmd::Quest3,
        background: Background::Black,
        display: DisplayPresence::Present,
        purpose: DisplayPurpose::Work,
        panel_count: V1_PANEL_COUNT,
    };
}

impl Default for V1Contract {
    fn default() -> Self {
        Self::V1
    }
}

/// 要求された機能が v1 の版図に収まっているか調べる。
///
/// 範囲外を一つでも含んでいれば `Unsupported` の封筒で返す。後続 phase が
/// 「ついでに」範囲を広げたときに、ここで止まる。
pub fn check_v1_scope(requested: &[Feature]) -> Result<()> {
    for feature in requested {
        if feature.excluded_from_v1() {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "feature is outside the v1 scope frozen by schorl.spec",
                TraceId::unattributed(),
            )
            .with_detail("feature", feature.as_str()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_contract_matches_the_pins() {
        let c = V1Contract::V1;
        assert_eq!(c.host, HostPlatform::Linux);
        assert_eq!(c.deliverable, Deliverable::SelfBuiltVrWorkEnvironment);
        assert_eq!(c.hmd, Hmd::Quest3);
        assert_eq!(c.background, Background::Black);
        assert_eq!(c.display, DisplayPresence::Present);
        assert_eq!(c.purpose, DisplayPurpose::Work);
        assert_eq!(c.panel_count, 1);
    }

    #[test]
    fn black_background_clears_to_opaque_black() {
        assert_eq!(Background::Black.clear_color_rgba(), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn excluded_and_included_features_partition_the_enum() {
        for feature in Feature::ALL {
            let excluded = V1_EXCLUDED_FEATURES.contains(&feature);
            let included = V1_INCLUDED_FEATURES.contains(&feature);
            assert!(
                excluded ^ included,
                "{} must be on exactly one side",
                feature.as_str()
            );
            assert_eq!(excluded, feature.excluded_from_v1());
        }
    }

    #[test]
    fn every_excluded_feature_is_refused() {
        for feature in V1_EXCLUDED_FEATURES {
            let err = check_v1_scope(&[feature])
                .expect_err("an excluded feature must not pass the scope check");
            assert_eq!(err.code(), ErrorCode::Unsupported);
        }
    }

    #[test]
    fn the_v1_feature_set_passes() {
        check_v1_scope(&V1_INCLUDED_FEATURES).expect("v1 features are in scope");
    }
}
