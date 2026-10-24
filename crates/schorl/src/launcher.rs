//! XDG Desktop Entry から VR 内 launcher に並べる application を作る。
//!
//! `Exec` は shell command ではないので shell を起動しない。quoted argv へ分け、
//! file / URL field code は「対象なし」として落としてから [`std::process::Command`] へ
//! 直接渡す。未知の field code を含む entry は specification に従って拒む。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use schorl_core::error::{Error, ErrorCode, Result as CoreResult};
use schorl_core::id::TraceId;

/// launcher に出す application 一つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherEntry {
    /// 表示名。
    pub name: String,
    /// shell を介さず起動する argv。
    pub argv: Vec<String>,
    /// 読み元。
    pub desktop_file: PathBuf,
    /// Browser / Terminal を先頭へ出すための小さな順位。
    rank: u8,
}

impl LauncherEntry {
    /// app を schorl の環境を継いだ子として起こす。
    pub fn spawn(&self) -> CoreResult<Child> {
        let (program, args) = self.argv.split_first().ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidArgument,
                "desktop entry has no program",
                TraceId::unattributed(),
            )
        })?;
        let mut command = Command::new(program);
        command.args(args);
        add_browser_isolation(&mut command, program);
        command.spawn().map_err(|e| {
            Error::new(
                ErrorCode::HostRefused,
                "could not start an application from the VR launcher",
                TraceId::unattributed(),
            )
            .with_detail("program", program.as_str())
            .with_detail("desktop_file", self.desktop_file.display().to_string())
            .caused_by(e)
        })
    }
}

/// XDG data directories から application を集める。
pub fn discover() -> Vec<LauncherEntry> {
    discover_in(&application_directories())
}

/// 指定 directory 群から application を集める。先にある同名 desktop file が勝つ。
pub fn discover_in(directories: &[PathBuf]) -> Vec<LauncherEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for directory in directories {
        let Ok(read_dir) = std::fs::read_dir(directory) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = read_dir
            .filter_map(|item| item.ok().map(|item| item.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "desktop"))
            .collect();
        paths.sort();
        for path in paths {
            let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !seen.insert(id.to_owned()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(Some(entry)) = parse_desktop_entry(&text, &path) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name)));
    entries
}

fn application_directories() -> Vec<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let mut roots = data_home.into_iter().collect::<Vec<_>>();
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
    roots.extend(
        system
            .split(':')
            .filter(|part| !part.is_empty())
            .map(PathBuf::from),
    );
    roots
        .into_iter()
        .map(|root| root.join("applications"))
        .collect()
}

fn parse_desktop_entry(text: &str, path: &Path) -> Result<Option<LauncherEntry>, String> {
    let mut in_main = false;
    let mut name = None;
    let mut exec = None;
    let mut entry_type = None;
    let mut hidden = false;
    let mut no_display = false;
    let mut categories = String::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Name" => name = Some(value.to_owned()),
            "Exec" => exec = Some(value.to_owned()),
            "Type" => entry_type = Some(value.to_owned()),
            "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            "Categories" => categories = value.to_owned(),
            _ => {}
        }
    }
    if entry_type.as_deref() != Some("Application") || hidden || no_display {
        return Ok(None);
    }
    let name = name.ok_or_else(|| "Desktop Entry has no Name".to_owned())?;
    let exec = exec.ok_or_else(|| "Desktop Entry has no Exec".to_owned())?;
    let argv = expand_exec(&exec, &name, path)?;
    if argv.is_empty() {
        return Err("Desktop Entry Exec expanded to no program".to_owned());
    }
    let haystack = format!("{};{};{}", name, categories, argv[0]).to_ascii_lowercase();
    let rank = if ["browser", "webbrowser", "chromium", "firefox"]
        .iter()
        .any(|word| haystack.contains(word))
    {
        0
    } else if ["terminal", "terminalemulator", "foot", "kitty", "alacritty"]
        .iter()
        .any(|word| haystack.contains(word))
    {
        1
    } else {
        2
    };
    Ok(Some(LauncherEntry {
        name,
        argv,
        desktop_file: path.to_owned(),
        rank,
    }))
}

fn expand_exec(exec: &str, name: &str, path: &Path) -> Result<Vec<String>, String> {
    let words = split_exec(exec)?;
    let mut out = Vec::new();
    for word in words {
        if word == "%i" {
            continue;
        }
        let mut expanded = String::new();
        let mut chars = word.chars();
        while let Some(ch) = chars.next() {
            if ch != '%' {
                expanded.push(ch);
                continue;
            }
            let code = chars
                .next()
                .ok_or_else(|| "Desktop Entry Exec ends with %".to_owned())?;
            match code {
                '%' => expanded.push('%'),
                'c' => expanded.push_str(name),
                'k' => expanded.push_str(path.to_string_lossy().as_ref()),
                // Launcher から file / URL を渡していないので空へ展開する。
                'f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm' => {}
                // %i は二引数への展開だけが有効。icon を使わないので単独なら上で除去。
                'i' => return Err("%i is only valid as its own argument".to_owned()),
                unknown => {
                    return Err(format!(
                        "Desktop Entry Exec has unknown field code %{unknown}"
                    ));
                }
            }
        }
        if !expanded.is_empty() {
            out.push(expanded);
        }
    }
    Ok(out)
}

fn split_exec(exec: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in exec.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch.is_whitespace() && !quoted {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if escaped || quoted {
        return Err("Desktop Entry Exec has invalid quoting".to_owned());
    }
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

fn add_browser_isolation(command: &mut Command, program: &str) {
    let executable = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    if executable.contains("chromium") || executable.contains("chrome") {
        command.arg("--ozone-platform=wayland");
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            command.arg(format!(
                "--user-data-dir={}",
                PathBuf::from(runtime).join("schorl-chromium").display()
            ));
        }
    }
}

/// 1280x720 XRGB8888 の launcher 画面を描く。
pub fn paint(entries: &[LauncherEntry], width: u32, height: u32) -> Vec<u8> {
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    fill(&mut pixels, width, 0, 0, width, height, [20, 16, 12]);
    draw_text(
        &mut pixels,
        width,
        42,
        28,
        "SCHORL LAUNCHER",
        4,
        [255, 235, 190],
    );
    draw_text(
        &mut pixels,
        width,
        44,
        72,
        "CLICK AN APP   F8 NEXT WINDOW",
        2,
        [170, 190, 210],
    );
    let columns = 3u32;
    let card_w = (width - 80) / columns;
    let card_h = 135u32;
    for (index, entry) in entries.iter().take(12).enumerate() {
        let column = index as u32 % columns;
        let row = index as u32 / columns;
        let x = 30 + column * card_w;
        let y = 115 + row * card_h;
        fill(
            &mut pixels,
            width,
            x,
            y,
            card_w - 18,
            card_h - 18,
            [64, 52, 42],
        );
        fill(&mut pixels, width, x, y, 8, card_h - 18, [230, 150, 55]);
        draw_text(
            &mut pixels,
            width,
            x + 24,
            y + 24,
            &format!("{:02} {}", index + 1, entry.name.to_ascii_uppercase()),
            2,
            [245, 245, 240],
        );
    }
    pixels
}

/// launcher 上の座標から entry を選ぶ。
pub fn entry_at(x: f64, y: f64, count: usize) -> Option<usize> {
    let column = ((x - 30.0) / 400.0).floor() as isize;
    let row = ((y - 115.0) / 135.0).floor() as isize;
    if !(0..3).contains(&column) || !(0..4).contains(&row) {
        return None;
    }
    let within_x = (x - 30.0) % 400.0;
    let within_y = (y - 115.0) % 135.0;
    if !(0.0..382.0).contains(&within_x) || !(0.0..117.0).contains(&within_y) {
        return None;
    }
    let index = row as usize * 3 + column as usize;
    (index < count.min(12)).then_some(index)
}

fn fill(pixels: &mut [u8], stride_px: u32, x: u32, y: u32, w: u32, h: u32, rgb: [u8; 3]) {
    for py in y..y.saturating_add(h) {
        for px in x..x.saturating_add(w).min(stride_px) {
            let at = ((py * stride_px + px) * 4) as usize;
            if at + 3 < pixels.len() {
                pixels[at..at + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 0xff]);
            }
        }
    }
}

fn draw_text(pixels: &mut [u8], stride: u32, x: u32, y: u32, text: &str, scale: u32, rgb: [u8; 3]) {
    let mut cursor = x;
    for ch in text.chars().take(38) {
        let glyph = glyph(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    fill(
                        pixels,
                        stride,
                        cursor + column * scale,
                        y + row as u32 * scale,
                        scale,
                        scale,
                        rgb,
                    );
                }
            }
        }
        cursor += 6 * scale;
    }
}

fn glyph(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 12, 12],
        '/' => [1, 2, 2, 4, 8, 8, 16],
        ':' => [0, 12, 12, 0, 12, 12, 0],
        ' ' => [0; 7],
        _ => [14, 17, 2, 4, 4, 0, 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_exec_is_argv_not_a_shell_command() {
        let text = "[Desktop Entry]\nType=Application\nName=Web Browser\nCategories=Network;WebBrowser;\nExec=chromium --new-window %U\n";
        let entry = parse_desktop_entry(text, Path::new("/tmp/browser.desktop"))
            .expect("valid entry")
            .expect("visible app");
        assert_eq!(entry.argv, ["chromium", "--new-window"]);
        assert_eq!(entry.rank, 0);
    }

    #[test]
    fn unknown_exec_field_codes_are_rejected() {
        assert!(expand_exec("app %Z", "App", Path::new("/tmp/app.desktop")).is_err());
    }

    #[test]
    fn desktop_exec_quoted_arguments_stay_single_arguments() {
        let argv = expand_exec(
            "app --title \"hello world\" --literal=%%",
            "App",
            Path::new("/tmp/app.desktop"),
        )
        .expect("valid quoting");
        assert_eq!(argv, ["app", "--title", "hello world", "--literal=%"]);
    }

    #[test]
    fn launcher_hit_test_uses_the_same_grid_as_paint() {
        assert_eq!(entry_at(40.0, 125.0, 12), Some(0));
        assert_eq!(entry_at(440.0, 260.0, 12), Some(4));
        assert_eq!(entry_at(1279.0, 719.0, 12), None);
    }

    #[test]
    fn launcher_pixels_are_xrgb_sized() {
        let pixels = paint(&[], 1280, 720);
        assert_eq!(pixels.len(), 1280 * 720 * 4);
    }
}
