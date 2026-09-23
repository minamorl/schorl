//! クライアントが描いた絵が swapchain まで届いたかを見分けるための語彙。
//!
//! # なぜ純色の四分割なのか
//!
//! `pin verify.machine_scope` の `client_frame_reaches_swapchain` は
//! 「クライアントの一枚が swapchain まで届くこと」であって「何かが描かれた」
//! ではない。**自前で作った絵と区別できなければ、この項目は測れていない。**
//!
//! そこで見分けの目印を、こちら (compositor / 描画側) ではなくクライアント側に
//! 決めさせる。クライアントは起動のたびに 7 色から 4 色を選び、その並びを
//! 標準出力へ申告してから描く。検証側は申告を読み、swapchain から読み戻した絵の
//! 中にその並びが出るかを見る。並びは 7·6·5·4 = 840 通りあるので、
//! 描画側が中身を知らずに当てることはできない。
//!
//! # なぜ成分が 0 か 255 の色だけなのか
//!
//! クライアントのバッファは `B8G8R8A8_UNORM` として読まれ、swapchain は
//! `B8G8R8A8_SRGB` である。書き込みのとき線形→sRGB の符号化が掛かるので、
//! 中間の値は必ず変わる。しかし符号化は 0 を 0 に、1 を 1 に写すので、
//! 成分が 0 か 255 だけの色は**画素の値がそのまま残る**。
//! sampler は `LINEAR` なので境目は混ざるが、区画の内側は混ざらない。
//!
//! この module は周囲効果を一つも持たない。全部ここで単体試験できる。

/// 成分が 0 か 255 だけでできた、黒でない色。
///
/// 7 枝あるのは 2³ − 1 (黒を除く) だからで、選択ではなく数え上げである。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PureColour {
    /// 赤。
    Red,
    /// 緑。
    Green,
    /// 青。
    Blue,
    /// シアン。
    Cyan,
    /// マゼンタ。
    Magenta,
    /// 黄。
    Yellow,
    /// 白。
    White,
}

impl PureColour {
    /// 7 色すべて。
    pub const ALL: [PureColour; 7] = [
        PureColour::Red,
        PureColour::Green,
        PureColour::Blue,
        PureColour::Cyan,
        PureColour::Magenta,
        PureColour::Yellow,
        PureColour::White,
    ];

    /// 申告に使う一文字。
    pub const fn letter(self) -> char {
        match self {
            PureColour::Red => 'R',
            PureColour::Green => 'G',
            PureColour::Blue => 'B',
            PureColour::Cyan => 'C',
            PureColour::Magenta => 'M',
            PureColour::Yellow => 'Y',
            PureColour::White => 'W',
        }
    }

    /// 一文字から戻す。知らない文字は `None`。
    pub const fn from_letter(letter: char) -> Option<Self> {
        match letter {
            'R' => Some(PureColour::Red),
            'G' => Some(PureColour::Green),
            'B' => Some(PureColour::Blue),
            'C' => Some(PureColour::Cyan),
            'M' => Some(PureColour::Magenta),
            'Y' => Some(PureColour::Yellow),
            'W' => Some(PureColour::White),
            _ => None,
        }
    }

    /// `[R, G, B]` の 8bit 成分。どれも 0 か 255 しか取らない。
    pub const fn rgb(self) -> [u8; 3] {
        match self {
            PureColour::Red => [255, 0, 0],
            PureColour::Green => [0, 255, 0],
            PureColour::Blue => [0, 0, 255],
            PureColour::Cyan => [0, 255, 255],
            PureColour::Magenta => [255, 0, 255],
            PureColour::Yellow => [255, 255, 0],
            PureColour::White => [255, 255, 255],
        }
    }

    /// `wl_shm` の `XRGB8888` / Vulkan の `B8G8R8A8` が並べる 4 byte。
    ///
    /// どちらも little endian の 32bit 語で byte 0 が B、1 が G、2 が R である。
    pub const fn bgrx(self) -> [u8; 4] {
        let [r, g, b] = self.rgb();
        [b, g, r, 0xff]
    }

    /// 読み戻した 4 byte がこの色ちょうどか。alpha は見ない。
    pub const fn matches_bgrx(self, bytes: [u8; 4]) -> bool {
        let [r, g, b] = self.rgb();
        bytes[0] == b && bytes[1] == g && bytes[2] == r
    }

    /// 読み戻した 4 byte がどの純色か。どれでもなければ `None`。
    pub fn of_bgrx(bytes: [u8; 4]) -> Option<Self> {
        PureColour::ALL.into_iter().find(|c| c.matches_bgrx(bytes))
    }
}

/// 四分割の並び。要素は左上・右上・左下・右下の順。
///
/// 座標は**クライアントが描く絵の中での**左上・右上である。描画側の幾何は
/// 絵の左上を画面の左上へ写すので (頂点 shader の `v_uv = (x+0.5, 0.5-y)` と、
/// `tanAngleDown - tanAngleUp` で y が下向きになる Vulkan 用の射影)、
/// 読み戻した絵でも並びは同じ向きで出る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuadPattern {
    /// 左上・右上・左下・右下。
    pub quadrants: [PureColour; 4],
}

impl QuadPattern {
    /// 四文字から。長さが 4 でない・知らない文字・重複があれば `None`。
    pub fn from_letters(letters: &str) -> Option<Self> {
        let mut found = [PureColour::Red; 4];
        let mut count = 0usize;
        for letter in letters.chars() {
            if count == 4 {
                return None;
            }
            found[count] = PureColour::from_letter(letter)?;
            count += 1;
        }
        if count != 4 {
            return None;
        }
        let pattern = Self { quadrants: found };
        pattern.is_distinct().then_some(pattern)
    }

    /// 四文字の綴り。
    pub fn to_letters(self) -> String {
        self.quadrants.iter().map(|c| c.letter()).collect()
    }

    /// 四区画の色が互いに違うか。
    pub fn is_distinct(self) -> bool {
        let mut sorted = self.quadrants;
        sorted.sort_unstable();
        sorted.windows(2).all(|w| w[0] != w[1])
    }

    /// この並びに使われている色を含むか。
    pub fn uses(self, colour: PureColour) -> bool {
        self.quadrants.contains(&colour)
    }

    /// 重複の無い並びをすべて数え上げる。7·6·5·4 = 840 通り。
    ///
    /// 検証側が「照合器が一つの答えしか返さない物ではない」ことを示すために、
    /// 申告以外の並びを引いてくる先である。
    pub fn all_distinct() -> Vec<QuadPattern> {
        let mut out = Vec::with_capacity(840);
        for a in PureColour::ALL {
            for b in PureColour::ALL {
                for c in PureColour::ALL {
                    for d in PureColour::ALL {
                        let pattern = QuadPattern {
                            quadrants: [a, b, c, d],
                        };
                        if pattern.is_distinct() {
                            out.push(pattern);
                        }
                    }
                }
            }
        }
        out
    }
}

/// 読み戻した絵の中で、各純色が何画素あり重心がどこかの数え上げ。
#[derive(Debug, Clone, PartialEq)]
pub struct ColourCensus {
    /// [`PureColour::ALL`] と同じ並びの画素数。
    pub counts: [usize; 7],
    /// 同じ並びの重心 (x, y)。一画素も無ければ `None`。
    pub centroids: [Option<(f64, f64)>; 7],
    /// 黒でない画素の総数。
    pub non_black_pixels: usize,
    /// 見た画素の総数。
    pub total_pixels: usize,
}

/// `B8G8R8A8` で読み戻した絵を数える。重心は画素の (x, y) で出る。
///
/// `bytes` は 1 画素 4 byte で詰まっていること (`copy_image_to_host` は詰めて返す)。
pub fn census_bgra(bytes: &[u8], width: u32) -> ColourCensus {
    let width = width.max(1) as usize;
    let mut counts = [0usize; 7];
    let mut sums = [(0.0f64, 0.0f64); 7];
    let mut non_black_pixels = 0usize;
    let mut total_pixels = 0usize;
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let pixel = [chunk[0], chunk[1], chunk[2], chunk[3]];
        if pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0 {
            non_black_pixels += 1;
        }
        if let Some(colour) = PureColour::of_bgrx(pixel) {
            let slot = colour as usize;
            counts[slot] += 1;
            sums[slot].0 += (index % width) as f64;
            sums[slot].1 += (index / width) as f64;
        }
        total_pixels += 1;
    }
    let mut centroids = [None; 7];
    for (slot, centroid) in centroids.iter_mut().enumerate() {
        if counts[slot] > 0 {
            *centroid = Some((
                sums[slot].0 / counts[slot] as f64,
                sums[slot].1 / counts[slot] as f64,
            ));
        }
    }
    ColourCensus {
        counts,
        centroids,
        non_black_pixels,
        total_pixels,
    }
}

/// 照合の閾値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchThresholds {
    /// 一区画がこれ以上の画素数で出ていれば「在る」と数える。
    pub present_at_least: usize,
    /// 使われていない色がこれを超えて出たら、読み取りが濁っていると見なす。
    pub absent_at_most: usize,
}

impl MatchThresholds {
    /// 既定。板が画面の何割を占めるかに依らない程度に低く、
    /// sampler の `LINEAR` が境目に作る混色が越えない程度に高い。
    pub const DEFAULT: MatchThresholds = MatchThresholds {
        present_at_least: 200,
        absent_at_most: 40,
    };
}

/// 照合が断った理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternMiss {
    /// 十分な画素を持つ純色がちょうど 4 色ではなかった。
    NotFourColours {
        /// 見つかった色数。
        found: usize,
    },
    /// 使われていないはずの色が閾値を越えて出た。
    StrayColour {
        /// その色の綴り。
        colour: char,
        /// 画素数。
        pixels: usize,
    },
    /// 四色が四分割に一つずつ収まらなかった。
    NotAQuartering,
}

/// 数え上げから四分割の並びを読む。
///
/// 中心は四色の重心の平均に取る。板が画面のどこに在っても、四色が四分割で
/// 並んでいる限り同じ答えになる。
pub fn read_pattern(
    census: &ColourCensus,
    thresholds: MatchThresholds,
) -> Result<QuadPattern, PatternMiss> {
    let present: Vec<(PureColour, (f64, f64))> = PureColour::ALL
        .into_iter()
        .filter_map(|colour| {
            let slot = colour as usize;
            if census.counts[slot] >= thresholds.present_at_least {
                census.centroids[slot].map(|centre| (colour, centre))
            } else {
                None
            }
        })
        .collect();
    if present.len() != 4 {
        return Err(PatternMiss::NotFourColours {
            found: present.len(),
        });
    }
    for colour in PureColour::ALL {
        let slot = colour as usize;
        let chosen = present.iter().any(|(c, _)| *c == colour);
        if !chosen && census.counts[slot] > thresholds.absent_at_most {
            return Err(PatternMiss::StrayColour {
                colour: colour.letter(),
                pixels: census.counts[slot],
            });
        }
    }

    let mid_x = present.iter().map(|(_, c)| c.0).sum::<f64>() / 4.0;
    let mid_y = present.iter().map(|(_, c)| c.1).sum::<f64>() / 4.0;
    let mut slots: [Option<PureColour>; 4] = [None; 4];
    for (colour, centre) in present {
        let left = centre.0 < mid_x;
        let top = centre.1 < mid_y;
        let index = match (top, left) {
            (true, true) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (false, false) => 3,
        };
        if slots[index].is_some() {
            return Err(PatternMiss::NotAQuartering);
        }
        slots[index] = Some(colour);
    }
    match slots {
        [Some(a), Some(b), Some(c), Some(d)] => Ok(QuadPattern {
            quadrants: [a, b, c, d],
        }),
        _ => Err(PatternMiss::NotAQuartering),
    }
}

/// 四分割の絵を `B8G8R8A8` で描く。
///
/// クライアント側がバッファを埋めるのにも、検証側が「照合器は入力で答えを
/// 変える」ことを示す囮を組むのにも同じ関数を使う。**同じ関数だから囮になる。**
pub fn paint_quadrants(pattern: QuadPattern, width: u32, height: u32) -> Vec<u8> {
    let width = width.max(1);
    let height = height.max(1);
    let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height {
        for x in 0..width {
            let top = y < height / 2;
            let left = x < width / 2;
            let index = match (top, left) {
                (true, true) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (false, false) => 3,
            };
            out.extend_from_slice(&pattern.quadrants[index].bgrx());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pure_colour_has_components_that_survive_srgb_encoding() {
        for colour in PureColour::ALL {
            for component in colour.rgb() {
                assert!(
                    component == 0 || component == 255,
                    "{colour:?} has a component the sRGB write would move"
                );
            }
        }
    }

    #[test]
    fn letters_round_trip() {
        for colour in PureColour::ALL {
            assert_eq!(PureColour::from_letter(colour.letter()), Some(colour));
        }
        assert_eq!(PureColour::from_letter('Z'), None);
    }

    #[test]
    fn there_are_eight_hundred_and_forty_distinct_arrangements() {
        assert_eq!(QuadPattern::all_distinct().len(), 7 * 6 * 5 * 4);
    }

    #[test]
    fn a_pattern_round_trips_through_its_letters() {
        let pattern = QuadPattern::from_letters("RGBW").expect("distinct");
        assert_eq!(pattern.to_letters(), "RGBW");
        assert_eq!(QuadPattern::from_letters("RRGB"), None, "repeats are not a pattern");
        assert_eq!(QuadPattern::from_letters("RGB"), None);
        assert_eq!(QuadPattern::from_letters("RGBWW"), None);
    }

    #[test]
    fn a_painted_quartering_reads_back_as_itself() {
        let pattern = QuadPattern::from_letters("RGBW").expect("distinct");
        let bytes = paint_quadrants(pattern, 64, 48);
        let census = census_bgra(&bytes, 64);
        assert_eq!(
            read_pattern(&census, MatchThresholds::DEFAULT),
            Ok(pattern)
        );
    }

    #[test]
    fn the_reader_returns_the_arrangement_it_was_given_not_a_fixed_answer() {
        // 照合器が「常に一致を返す物」でないことの較正。囮を描いて読ませ、
        // 囮が返ることを見る。返る値が入力で変わるなら定数ではない。
        let announced = QuadPattern::from_letters("RGBW").expect("distinct");
        let mut differed = 0usize;
        for decoy in QuadPattern::all_distinct() {
            let bytes = paint_quadrants(decoy, 32, 32);
            let census = census_bgra(&bytes, 32);
            let read = read_pattern(&census, MatchThresholds::DEFAULT).expect("a quartering");
            assert_eq!(read, decoy, "the reader must follow its input");
            if decoy != announced {
                assert_ne!(read, announced);
                differed += 1;
            }
        }
        assert_eq!(differed, 839);
    }

    #[test]
    fn an_all_black_image_is_not_a_pattern() {
        let bytes = vec![0u8; 64 * 48 * 4];
        let census = census_bgra(&bytes, 64);
        assert_eq!(
            read_pattern(&census, MatchThresholds::DEFAULT),
            Err(PatternMiss::NotFourColours { found: 0 })
        );
    }

    #[test]
    fn a_single_colour_field_is_not_a_quartering() {
        let mut bytes = Vec::new();
        for _ in 0..(32 * 32) {
            bytes.extend_from_slice(&PureColour::Red.bgrx());
        }
        let census = census_bgra(&bytes, 32);
        assert_eq!(
            read_pattern(&census, MatchThresholds::DEFAULT),
            Err(PatternMiss::NotFourColours { found: 1 })
        );
    }

    #[test]
    fn a_stray_fifth_colour_is_refused() {
        let pattern = QuadPattern::from_letters("RGBW").expect("distinct");
        let mut bytes = paint_quadrants(pattern, 64, 64);
        // 使っていない色を閾値より多く撒く。
        for pixel in 0..(MatchThresholds::DEFAULT.absent_at_most + 5) {
            let base = pixel * 4;
            bytes[base..base + 4].copy_from_slice(&PureColour::Cyan.bgrx());
        }
        let census = census_bgra(&bytes, 64);
        match read_pattern(&census, MatchThresholds::DEFAULT) {
            Err(PatternMiss::StrayColour { colour, .. }) => assert_eq!(colour, 'C'),
            other => panic!("expected a stray colour, got {other:?}"),
        }
    }

}
