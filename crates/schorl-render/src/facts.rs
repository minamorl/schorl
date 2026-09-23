//! 観測できた事実と、それを出す口。
//!
//! `pin verify.machine_scope` は機械が確かめられる範囲を五点に固定している。
//! そのうちこの crate が触るのは `build_passes` / `openxr_session_opens` /
//! `client_frame_reaches_swapchain` / `unit_tests` の四点である。
//!
//! **`hmd_accepted` を表す欄はここに無い。** `pin verify.no_green_substitute`
//! (`forbid schorl.machine_green = substitute_for_hmd_acceptance`) と
//! `property verify.green_is_not_acceptance` があるので、機械の緑が実機受け入れを
//! 名乗れる場所を作らない。型に無ければ書けない。

use schorl_core::error::Result;
use schorl_core::json::JsonValue;
use schorl_core::log::{LogRecord, LogSink};
use std::io::Write as _;
use std::sync::Mutex;

/// テクスチャがどの経路で載ったか。
///
/// `free schorl.compositor.buffer_import_path` の二つの選択肢に対応する。
/// 片方しか無い構成にしないという判断を、数える型として持っている。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextureRoute {
    /// `VK_EXT_external_memory_dma_buf` で import した。
    Dmabuf,
    /// CPU 側の画素 (shm 相当) を staging buffer 経由で載せた。
    Shm,
}

impl TextureRoute {
    /// 報告に出す綴り。
    pub const fn as_str(self) -> &'static str {
        match self {
            TextureRoute::Dmabuf => "dmabuf",
            TextureRoute::Shm => "shm",
        }
    }
}

/// 描いて出したことについて機械が言い切れる事実。
///
/// 要約ではなく観測を報告へ貼るために在る。「submit できた」ではなく
/// 「何枚 submit したか」「そのうち何枚がサーフェスを含んでいたか」を数字で持つ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderFacts {
    /// `xrEndFrame` まで到達したフレームの数。
    pub frames_submitted: usize,
    /// `should_render` が偽で、層を出さずに閉じたフレームの数。
    pub frames_skipped: usize,
    /// サーフェスを一枚以上含んだフレームの数。
    pub frames_with_surface: usize,
    /// 実際に描いたサーフェスの総数 (フレームをまたいで積む)。
    pub surface_draws: usize,
    /// dmabuf 経路のテクスチャを描いた回数。
    pub dmabuf_draws: usize,
    /// shm 経路のテクスチャを描いた回数。
    pub shm_draws: usize,
    /// 背景を黒で clear した回数 (view ごとに数える)。
    pub black_clears: usize,
    /// 作った swapchain の数 (view ごとに一つ)。
    pub swapchains_created: usize,
    /// 各 swapchain がランタイムから受け取った画像の数。
    pub swapchain_image_counts: Vec<u32>,
    /// swapchain の一枚の寸法 (幅, 高さ)。
    pub swapchain_extent: Option<(u32, u32)>,
    /// 実際に使った `VkFormat` の生の値。
    pub swapchain_format: Option<u32>,
}

impl RenderFacts {
    /// 空の帳面。
    pub fn new() -> Self {
        Self::default()
    }

    /// ログや報告に貼れる json。
    ///
    /// `code.log.format` の json 一行とは別物で、こちらは `msg` の中身ではなく
    /// 事実そのものである。秘密は入らない (`code.log.no_secrets`)。
    pub fn to_json(&self) -> JsonValue {
        let mut fields: Vec<(String, JsonValue)> = vec![
            (
                "frames_submitted".into(),
                JsonValue::Int(self.frames_submitted as i64),
            ),
            (
                "frames_skipped".into(),
                JsonValue::Int(self.frames_skipped as i64),
            ),
            (
                "frames_with_surface".into(),
                JsonValue::Int(self.frames_with_surface as i64),
            ),
            (
                "surface_draws".into(),
                JsonValue::Int(self.surface_draws as i64),
            ),
            (
                "dmabuf_draws".into(),
                JsonValue::Int(self.dmabuf_draws as i64),
            ),
            ("shm_draws".into(), JsonValue::Int(self.shm_draws as i64)),
            (
                "black_clears".into(),
                JsonValue::Int(self.black_clears as i64),
            ),
            (
                "swapchains_created".into(),
                JsonValue::Int(self.swapchains_created as i64),
            ),
            (
                "swapchain_image_counts".into(),
                JsonValue::Array(
                    self.swapchain_image_counts
                        .iter()
                        .map(|c| JsonValue::Int(i64::from(*c)))
                        .collect(),
                ),
            ),
        ];
        if let Some((width, height)) = self.swapchain_extent {
            fields.push(("swapchain_width".into(), JsonValue::Int(i64::from(width))));
            fields.push(("swapchain_height".into(), JsonValue::Int(i64::from(height))));
        }
        if let Some(format) = self.swapchain_format {
            fields.push(("swapchain_format".into(), JsonValue::Int(i64::from(format))));
        }
        JsonValue::Object(fields)
    }

    /// 一枚 submit したことを数える。
    pub fn record_submit(&mut self, surfaces: &[TextureRoute], views: usize) {
        self.frames_submitted += 1;
        self.black_clears += views;
        if !surfaces.is_empty() {
            self.frames_with_surface += 1;
        }
        // view ごとに同じサーフェスを描くので、draw 数は view 数を掛ける。
        for route in surfaces {
            self.surface_draws += views;
            match route {
                TextureRoute::Dmabuf => self.dmabuf_draws += views,
                TextureRoute::Shm => self.shm_draws += views,
            }
        }
    }

    /// 層を出さずに閉じたフレームを数える。
    pub fn record_skip(&mut self) {
        self.frames_skipped += 1;
    }
}

/// 標準出力へ json 一行ずつ出す行き先。
///
/// `code.log.format: require log.format = json` と
/// `code.log.required_fields: require log.fields in [ts, level, trace_id, msg]` は
/// [`LogRecord::to_line`] が満たしている。ここは書き出しの周囲効果だけを持つ。
#[derive(Debug, Default)]
pub struct StdoutLogSink {
    guard: Mutex<()>,
}

impl StdoutLogSink {
    /// 作る。
    pub fn new() -> Self {
        Self::default()
    }
}

impl LogSink for StdoutLogSink {
    fn emit(&self, record: &LogRecord) -> Result<()> {
        // 行が混ざらないように直列化する。poison しても行を落とさない
        // (ログの出力先が壊れても本題を止めない)。
        let _guard = self.guard.lock();
        let mut out = std::io::stdout().lock();
        let line = record.to_line();
        // 書き出しの失敗は封筒で返す。panic しない。
        writeln!(out, "{line}").map_err(|e| {
            schorl_core::Error::new(
                schorl_core::ErrorCode::Internal,
                "the log line could not be written to standard output",
                schorl_core::id::TraceId::unattributed(),
            )
            .caused_by(e)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_ledger_claims_nothing() {
        // 数えていないものを 0 でなく「主張しない」形に寄せる意味はここには無いが、
        // 起点が 0 であること自体は測っておく (勝手に緑から始まらない)。
        let facts = RenderFacts::new();
        assert_eq!(facts.frames_submitted, 0);
        assert_eq!(facts.frames_with_surface, 0);
        assert_eq!(facts.dmabuf_draws, 0);
        assert_eq!(facts.shm_draws, 0);
    }

    #[test]
    fn a_submit_with_no_surface_still_counts_the_black_clear() {
        // 背景の黒は毎フレーム自分が書く。サーフェスが無くても黒は出る。
        let mut facts = RenderFacts::new();
        facts.record_submit(&[], 2);
        assert_eq!(facts.frames_submitted, 1);
        assert_eq!(facts.black_clears, 2);
        assert_eq!(facts.frames_with_surface, 0);
        assert_eq!(facts.surface_draws, 0);
    }

    #[test]
    fn both_routes_are_counted_apart() {
        let mut facts = RenderFacts::new();
        facts.record_submit(&[TextureRoute::Dmabuf, TextureRoute::Shm], 2);
        assert_eq!(facts.frames_with_surface, 1);
        assert_eq!(facts.surface_draws, 4);
        assert_eq!(facts.dmabuf_draws, 2);
        assert_eq!(facts.shm_draws, 2);
    }

    #[test]
    fn more_than_one_surface_is_representable() {
        // pin v1.window_count = one_or_more。上限を型でも数でも置いていないこと。
        let mut facts = RenderFacts::new();
        let routes = vec![TextureRoute::Shm; 7];
        facts.record_submit(&routes, 1);
        assert_eq!(facts.surface_draws, 7);
    }

    #[test]
    fn the_ledger_json_carries_the_counts() {
        let mut facts = RenderFacts::new();
        facts.record_submit(&[TextureRoute::Dmabuf], 2);
        facts.swapchain_extent = Some((1280, 720));
        let rendered = facts.to_json().render();
        assert!(rendered.contains("\"frames_submitted\":1"), "{rendered}");
        assert!(rendered.contains("\"dmabuf_draws\":2"), "{rendered}");
        assert!(rendered.contains("\"swapchain_width\":1280"), "{rendered}");
    }

    #[test]
    fn the_route_spellings_are_stable() {
        assert_eq!(TextureRoute::Dmabuf.as_str(), "dmabuf");
        assert_eq!(TextureRoute::Shm.as_str(), "shm");
    }
}
