//! Hyprland の headless 仮想出力を、実行時だけ借りる実装。
//!
//! 満たす pin:
//! - `host.created_output_lifecycle: require schorl.created_virtual_output.lifetime
//!   = released_at_process_exit` — 作った出力は [`VirtualOutput`] が持ち、
//!   `Drop` でも明示解放でも `hyprctl output remove` が走る。
//! - `host.no_persistent_config_change: forbid schorl.host_compositor.persistent_configuration
//!   = modified` — 触るのは `hyprctl` の実行時要求だけで、設定ファイルは読み書きしない。
//!   [`HyprlandHeadlessOutputProvider::declared_host_ops`] は
//!   [`HostOpKind::Runtime`](crate::HostOpKind::Runtime) しか申告しない。
//! - `house.effect_boundary.form` / `house.effect_boundary.substitution` —
//!   外の世界に触るのは [`CommandRunner`] 一本だけで、試験では差し替えられる。
//! - `code.idempotency.write` — 同じ [`OutputRequest::idempotency_key`] で二枚目を
//!   作らない。
//!
//! `free schorl.display.provisioning_method` の中での選択なので、spec は
//! 「hyprctl を使え」とは言っていない。決まっているのは「作ったら返す」
//! 「恒久設定は触らない」の二つだけである。
//!
//! 一次資料: `hyprctl output --help` の逐語
//! `usage: hyprctl [flags] output <create <backend> | remove <name>>` /
//! `create <backend>: Creates new virtual output. Possible values for backend:
//! wayland, x11, headless or auto.` /
//! `remove <name>: Removes virtual output. Pass the output's name, as found in
//! 'hyprctl monitors'`。

use std::process::Command;
use std::sync::{Arc, Mutex};

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;

use crate::{HostOp, OutputId, OutputRelease, OutputRequest, VirtualOutput, VirtualOutputProvider};

/// 外部コマンドを一度走らせた結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// 終了コード。シグナルで落ちた場合は `None`。
    pub status_code: Option<i32>,
    /// 標準出力 (非 UTF-8 は失われる形で読む)。
    pub stdout: String,
    /// 標準エラー。
    pub stderr: String,
}

impl CommandOutput {
    /// 終了コード 0 で終わったか。
    pub fn is_success(&self) -> bool {
        self.status_code == Some(0)
    }
}

/// 外部コマンドを一つ走らせる capability。
///
/// これがこの module の唯一の周囲効果である
/// (`house.effect_boundary.no_god_capability`)。
pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    /// 走らせて、終わるまで待ち、結果を返す。
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput>;
}

/// `std::process::Command` で本当に走らせる実装。
#[derive(Debug, Clone, Default)]
pub struct SystemCommandRunner {
    env: Vec<(String, String)>,
}

impl SystemCommandRunner {
    /// 親の環境をそのまま継いで作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// 環境変数を一つ上書きする。
    ///
    /// `hyprctl` は `HYPRLAND_INSTANCE_SIGNATURE` と `XDG_RUNTIME_DIR` を見るので、
    /// 親の環境に無いときはここで足す。**秘密は渡さない** (`code.log.no_secrets` と
    /// 同じ理由で、この値はエラー封筒にも出さない)。
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput> {
        let mut command = Command::new(program);
        command.args(args);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let output = command.output().map_err(|source| {
            Error::new(
                ErrorCode::HostRefused,
                "failed to run the host compositor control command",
                TraceId::unattributed(),
            )
            .with_detail("program", program)
            .caused_by(source)
        })?;
        Ok(CommandOutput {
            status_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// `hyprctl monitors` の平文出力から出力名だけを読む。
///
/// 逐語の形は `Monitor HEADLESS-2 (ID 1):` である。JSON ではなく平文を読むのは、
/// この crate に JSON 構文解析器を持ち込まないため
/// (`schorl_core::json` は書き出し専用)。
pub fn parse_monitor_names(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("Monitor "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

/// `hyprctl -j instances` の出力から、指定 socket の instance signature を読む。
///
/// 逐語の形は `"instance": "<sig>"` と `"wl_socket": "wayland-1"` である。
/// 鍵と値だけを拾う狭い読み取りで、汎用の JSON 構文解析ではない。
pub fn parse_instance_signature(stdout: &str, wayland_display: &str) -> Option<String> {
    for chunk in stdout.split('{').skip(1) {
        let chunk = chunk.split('}').next().unwrap_or(chunk);
        let socket = json_string_field(chunk, "wl_socket");
        if socket.as_deref() == Some(wayland_display) {
            return json_string_field(chunk, "instance");
        }
    }
    None
}

fn json_string_field(chunk: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after_key = chunk.split_once(&needle)?.1;
    let after_colon = after_key.split_once(':')?.1;
    let after_quote = after_colon.split_once('"')?.1;
    let (value, _) = after_quote.split_once('"')?;
    Some(value.to_owned())
}

/// `hyprctl output remove <name>` で返す解放器。
///
/// 冪等である。既に消えている名前を渡されたら何もせず成功する
/// (`Drop` と明示解放の両方から呼ばれ得るため)。
#[derive(Debug)]
pub struct HyprctlOutputReleaser {
    runner: Arc<dyn CommandRunner>,
}

impl HyprctlOutputReleaser {
    /// 走らせ口を与えて作る。
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

impl OutputRelease for HyprctlOutputReleaser {
    fn release(&self, id: &OutputId) -> Result<()> {
        let listed = self.runner.run("hyprctl", &["monitors"])?;
        if listed.is_success()
            && !parse_monitor_names(&listed.stdout)
                .iter()
                .any(|n| n == id.as_str())
        {
            // もう無い。二度目の解放はここで静かに終わる。
            return Ok(());
        }
        let removed = self
            .runner
            .run("hyprctl", &["output", "remove", id.as_str()])?;
        if removed.is_success() {
            return Ok(());
        }
        Err(Error::new(
            ErrorCode::ResourceReleaseFailed,
            "hyprctl refused to remove the virtual output schorl created",
            TraceId::unattributed(),
        )
        .with_detail("output", id.as_str())
        .with_detail("stderr", first_line(&removed.stderr)))
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_owned()
}

/// Hyprland の headless 仮想出力を一枚借りる提供者。
#[derive(Debug)]
pub struct HyprlandHeadlessOutputProvider {
    runner: Arc<dyn CommandRunner>,
    served_keys: Mutex<Vec<String>>,
}

impl HyprlandHeadlessOutputProvider {
    /// 走らせ口を与えて作る。
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            runner,
            served_keys: Mutex::new(Vec::new()),
        }
    }

    /// 親の環境をそのまま使う本物の走らせ口で作る。
    pub fn with_system_runner() -> Self {
        Self::new(Arc::new(SystemCommandRunner::new()))
    }

    fn monitor_names(&self) -> Result<Vec<String>> {
        let listed = self.runner.run("hyprctl", &["monitors"])?;
        if !listed.is_success() {
            return Err(Error::new(
                ErrorCode::HostRefused,
                "hyprctl could not list the host compositor outputs",
                TraceId::unattributed(),
            )
            .with_detail("stderr", first_line(&listed.stderr)));
        }
        Ok(parse_monitor_names(&listed.stdout))
    }

    fn claim_idempotency_key(&self, request: &OutputRequest) -> Result<()> {
        let key = request.idempotency_key.as_str().to_owned();
        let mut served = match self.served_keys.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if served.iter().any(|seen| seen == &key) {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "this idempotency key already created an output; schorl keeps exactly one",
                TraceId::unattributed(),
            )
            .with_detail("idempotency_key", key));
        }
        served.push(key);
        Ok(())
    }
}

/// `hyprctl keyword monitor` へ渡す mode 文字列を組む。
///
/// 逐語の形は `<name>,<width>x<height>@<hz>,<position>,<scale>`。
/// `refresh_millihz` は mHz なので 1000 で割って Hz にする。
pub fn monitor_keyword_value(
    name: &str,
    width_px: u32,
    height_px: u32,
    refresh_millihz: u32,
) -> String {
    let hz = f64::from(refresh_millihz) / 1000.0;
    format!("{name},{width_px}x{height_px}@{hz},auto,1")
}

impl VirtualOutputProvider for HyprlandHeadlessOutputProvider {
    fn create(&self, request: &OutputRequest) -> Result<VirtualOutput> {
        if request.width_px == 0 || request.height_px == 0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "a virtual output must have a non-zero extent",
                TraceId::unattributed(),
            ));
        }
        if request.refresh_millihz == 0 {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "a virtual output must have a non-zero refresh rate",
                TraceId::unattributed(),
            ));
        }
        self.claim_idempotency_key(request)?;

        let before = self.monitor_names()?;
        let created = self
            .runner
            .run("hyprctl", &["output", "create", "headless"])?;
        if !created.is_success() {
            return Err(Error::new(
                ErrorCode::HostRefused,
                "hyprctl refused to create a headless output",
                TraceId::unattributed(),
            )
            .with_detail("stderr", first_line(&created.stderr)));
        }

        let after = self.monitor_names()?;
        let mut fresh: Vec<String> = after
            .into_iter()
            .filter(|name| !before.contains(name))
            .collect();
        let name = match fresh.len() {
            1 => fresh.remove(0),
            0 => {
                return Err(Error::new(
                    ErrorCode::HostRefused,
                    "hyprctl reported success but no new output appeared",
                    TraceId::unattributed(),
                ));
            }
            other => {
                // 自分の一枚を名指しできない。誰かの出力を掴む方が危ないので諦める。
                return Err(Error::new(
                    ErrorCode::HostRefused,
                    "more than one output appeared; schorl will not guess which one it owns",
                    TraceId::unattributed(),
                )
                .with_detail("appeared", other as i64));
            }
        };

        let id = OutputId::new(name)?;
        // ここから先の失敗は必ず出力を返してから返る
        // (`house.resource_lifecycle.release_paths` の error / early_return)。
        let held = VirtualOutput::new(
            id,
            Arc::new(HyprctlOutputReleaser::new(self.runner.clone())),
        );

        let keyword = monitor_keyword_value(
            held.id().as_str(),
            request.width_px,
            request.height_px,
            request.refresh_millihz,
        );
        let sized = self
            .runner
            .run("hyprctl", &["keyword", "monitor", &keyword])?;
        if !sized.is_success() {
            // `held` はここで落ちて `Drop` が出力を返す。
            return Err(Error::new(
                ErrorCode::HostRefused,
                "hyprctl refused the runtime mode for the output schorl created",
                TraceId::unattributed(),
            )
            .with_detail("keyword", keyword)
            .with_detail("stderr", first_line(&sized.stderr)));
        }

        Ok(held)
    }

    fn declared_host_ops(&self) -> Vec<HostOp> {
        vec![
            HostOp::runtime("hyprctl output create headless (runtime only, removed on exit)"),
            HostOp::runtime("hyprctl keyword monitor <name>,... (runtime keyword, no config file)"),
            HostOp::runtime("hyprctl output remove <name> (returns what schorl created)"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert_runtime_only;
    use schorl_core::id::{Id, IdScheme, IdempotencyKey};

    /// 台本どおりに答える贋物の走らせ口。呼ばれた引数も覚える。
    #[derive(Debug, Default)]
    struct ScriptedRunner {
        monitors: Mutex<Vec<String>>,
        calls: Mutex<Vec<String>>,
        create_succeeds: bool,
        keyword_succeeds: bool,
        appears_as: Option<String>,
    }

    impl ScriptedRunner {
        fn new(initial: &[&str], appears_as: Option<&str>) -> Self {
            Self {
                monitors: Mutex::new(initial.iter().map(|s| (*s).to_owned()).collect()),
                calls: Mutex::new(Vec::new()),
                create_succeeds: true,
                keyword_succeeds: true,
                appears_as: appears_as.map(str::to_owned),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock").clone()
        }

        fn monitor_listing(&self) -> String {
            self.monitors
                .lock()
                .expect("lock")
                .iter()
                .enumerate()
                .map(|(i, name)| format!("Monitor {name} (ID {i}):\n\t1920x1080@60.00 at 0x0\n"))
                .collect()
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput> {
            self.calls
                .lock()
                .expect("lock")
                .push(format!("{program} {}", args.join(" ")));
            let ok = |stdout: String| CommandOutput {
                status_code: Some(0),
                stdout,
                stderr: String::new(),
            };
            let fail = |stderr: &str| CommandOutput {
                status_code: Some(1),
                stdout: String::new(),
                stderr: stderr.to_owned(),
            };
            match args {
                ["monitors"] => Ok(ok(self.monitor_listing())),
                ["output", "create", "headless"] => {
                    if !self.create_succeeds {
                        return Ok(fail("no backend"));
                    }
                    if let Some(name) = &self.appears_as {
                        self.monitors.lock().expect("lock").push(name.clone());
                    }
                    Ok(ok(String::new()))
                }
                ["output", "remove", name] => {
                    self.monitors.lock().expect("lock").retain(|m| m != name);
                    Ok(ok(String::new()))
                }
                ["keyword", "monitor", _] => {
                    if self.keyword_succeeds {
                        Ok(ok(String::new()))
                    } else {
                        Ok(fail("invalid mode"))
                    }
                }
                _ => Ok(fail("unexpected command")),
            }
        }
    }

    fn request(key: &str) -> OutputRequest {
        OutputRequest {
            preferred_name: "SCHORL-PANEL".to_owned(),
            width_px: 1920,
            height_px: 1080,
            refresh_millihz: 60_000,
            idempotency_key: IdempotencyKey::new(Id::new(IdScheme::Ulid, key).expect("valid text")),
        }
    }

    #[test]
    fn monitor_names_come_from_the_plain_listing() {
        let listing = "Monitor HEADLESS-2 (ID 1):\n\t2560x1440@120.00000 at 0x0\n\tscale: 1.00\nMonitor HEADLESS-3 (ID 2):\n\tdescription: \n";
        assert_eq!(
            parse_monitor_names(listing),
            vec!["HEADLESS-2".to_owned(), "HEADLESS-3".to_owned()]
        );
    }

    #[test]
    fn the_instance_signature_is_matched_by_socket() {
        let stdout = r#"[
{
    "instance": "sig-a",
    "wl_socket": "wayland-0"
},
{
    "instance": "sig-b",
    "wl_socket": "wayland-1"
}
]"#;
        assert_eq!(
            parse_instance_signature(stdout, "wayland-1"),
            Some("sig-b".to_owned())
        );
        assert_eq!(parse_instance_signature(stdout, "wayland-9"), None);
    }

    #[test]
    fn the_mode_keyword_is_built_from_the_request() {
        assert_eq!(
            monitor_keyword_value("HEADLESS-3", 1920, 1080, 60_000),
            "HEADLESS-3,1920x1080@60,auto,1"
        );
    }

    #[test]
    fn dropping_the_handle_removes_only_the_output_schorl_created() {
        let runner = Arc::new(ScriptedRunner::new(&["HEADLESS-2"], Some("HEADLESS-3")));
        let provider = HyprlandHeadlessOutputProvider::new(runner.clone());
        {
            let output = provider.create(&request("key-1")).expect("created");
            assert_eq!(output.id().as_str(), "HEADLESS-3");
            assert!(
                runner
                    .monitors
                    .lock()
                    .expect("lock")
                    .contains(&"HEADLESS-2".to_owned()),
                "the output owned by another program is untouched"
            );
        }
        assert_eq!(
            runner.monitors.lock().expect("lock").clone(),
            vec!["HEADLESS-2".to_owned()],
            "only the created output is gone"
        );
        assert!(
            runner
                .calls()
                .iter()
                .any(|c| c == "hyprctl output remove HEADLESS-3"),
            "calls were {:?}",
            runner.calls()
        );
    }

    #[test]
    fn a_refused_mode_still_returns_the_output() {
        let mut scripted = ScriptedRunner::new(&["HEADLESS-2"], Some("HEADLESS-3"));
        scripted.keyword_succeeds = false;
        let runner = Arc::new(scripted);
        let provider = HyprlandHeadlessOutputProvider::new(runner.clone());
        let err = provider
            .create(&request("key-2"))
            .expect_err("mode refused");
        assert_eq!(err.code(), ErrorCode::HostRefused);
        assert_eq!(
            runner.monitors.lock().expect("lock").clone(),
            vec!["HEADLESS-2".to_owned()],
            "the half-built output was handed back"
        );
    }

    #[test]
    fn nothing_is_created_when_hyprctl_refuses() {
        let mut scripted = ScriptedRunner::new(&["HEADLESS-2"], None);
        scripted.create_succeeds = false;
        let runner = Arc::new(scripted);
        let provider = HyprlandHeadlessOutputProvider::new(runner.clone());
        let err = provider
            .create(&request("key-3"))
            .expect_err("create refused");
        assert_eq!(err.code(), ErrorCode::HostRefused);
        assert_eq!(
            runner.monitors.lock().expect("lock").clone(),
            vec!["HEADLESS-2".to_owned()]
        );
    }

    #[test]
    fn the_same_idempotency_key_does_not_create_a_second_output() {
        let runner = Arc::new(ScriptedRunner::new(&["HEADLESS-2"], Some("HEADLESS-3")));
        let provider = HyprlandHeadlessOutputProvider::new(runner.clone());
        let first = provider.create(&request("key-4")).expect("created");
        let err = provider
            .create(&request("key-4"))
            .expect_err("the same key must not create a second output");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
        drop(first);
    }

    #[test]
    fn releasing_twice_is_quiet() {
        let runner = Arc::new(ScriptedRunner::new(&["HEADLESS-2", "HEADLESS-7"], None));
        let releaser = HyprctlOutputReleaser::new(runner.clone());
        let id = OutputId::new("HEADLESS-7").expect("valid name");
        releaser.release(&id).expect("first release");
        releaser.release(&id).expect("second release is a no-op");
    }

    #[test]
    fn a_zero_sized_output_is_refused_before_touching_the_host() {
        let runner = Arc::new(ScriptedRunner::new(&["HEADLESS-2"], Some("HEADLESS-3")));
        let provider = HyprlandHeadlessOutputProvider::new(runner.clone());
        let mut req = request("key-5");
        req.width_px = 0;
        let err = provider.create(&req).expect_err("zero width");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
        assert!(runner.calls().is_empty(), "the host was never asked");
    }

    #[test]
    fn the_provider_declares_only_runtime_operations() {
        let provider = HyprlandHeadlessOutputProvider::with_system_runner();
        assert_runtime_only(&provider.declared_host_ops()).expect("runtime only");
    }
}
