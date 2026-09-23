//! shader を build 時に GLSL から SPIR-V へ焼く。
//!
//! なぜ build script か: `shaders/quad.vert` と `shaders/quad.frag` は source で、
//! `*.spv` はそこから機械が作れる生成物である。生成物を repo の中身にしないことは
//! pin public.no_build_artifacts (forbid schorl.repository.content = build_artifacts)
//! が禁じている。射程 (何が build artifact か) を待たずにどちらの読みでも満たすため、
//! `.spv` は tracked にせず `OUT_DIR` へ吐く。renderer は `OUT_DIR` を見る。
//!
//! 何を自由に決めたか: pin は build system も toolchain も縛っていない
//! (free schorl.build_system)。ここで選んだ glslangValidator / glslc という具体は
//! 要求ではなく実装の選択である。
//!
//! 生成器が居ない環境では **黙って壊れない**。何が足りないか・どう直すかを stderr へ
//! 書いて非零で落ちる。cargo は build script の stderr をそのまま見せるので、
//! 「shader が古い」ではなく「glslangValidator が無い」と読める形で止まる。

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// GLSL コンパイラを名指しする env。値は実行ファイルの名前か絶対パス。
const COMPILER_ENV: &str = "SCHORL_GLSL_COMPILER";
/// SPIR-V validator を名指しする env。空文字を渡すと検証を明示的に切る。
const VALIDATOR_ENV: &str = "SCHORL_SPIRV_VAL";
/// 探す既定の GLSL コンパイラ。先に見つかった方を使う。
const COMPILER_CANDIDATES: [&str; 2] = ["glslangValidator", "glslc"];
/// 探す既定の validator。
const VALIDATOR_CANDIDATES: [&str; 1] = ["spirv-val"];
/// 焼く対象。`(source, 生成物, stage)`。stage は glslc 用 (glslang は拡張子で判る)。
const SHADERS: [(&str, &str, &str); 2] = [
    ("shaders/quad.vert", "quad.vert.spv", "vertex"),
    ("shaders/quad.frag", "quad.frag.spv", "fragment"),
];
/// 焼く先の Vulkan 版。ash 0.38 の既定経路に合わせた最も広い下限。
const TARGET_ENV: &str = "vulkan1.0";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed={COMPILER_ENV}");
    println!("cargo::rerun-if-env-changed={VALIDATOR_ENV}");
    for (source, _, _) in SHADERS {
        println!("cargo::rerun-if-changed={source}");
    }

    let out_dir = PathBuf::from(
        env::var_os("OUT_DIR")
            .unwrap_or_else(|| fail("OUT_DIR が無い。cargo から呼ばれていない。")),
    );
    let compiler = find_tool(COMPILER_ENV, &COMPILER_CANDIDATES).unwrap_or_else(|| {
        fail(&missing_compiler_message());
    });
    let validator = find_tool(VALIDATOR_ENV, &VALIDATOR_CANDIDATES);
    if validator.is_none() {
        println!(
            "cargo::warning=spirv-val が見つからないので SPIR-V の検証を飛ばした。\
             入れる場合は spirv-tools (apt: spirv-tools)。{VALIDATOR_ENV} で名指しもできる。"
        );
    }

    for (source, artifact, stage) in SHADERS {
        let out = out_dir.join(artifact);
        compile(&compiler, source, &out, stage);
        if let Some(validator) = &validator {
            validate(validator, &out);
        }
    }
}

/// env の名指しを優先し、無ければ候補を PATH から探す。
///
/// 名指しされた物も PATH の候補も、実在を確かめてから返す。存在しない名前をそのまま
/// 渡すと「起動できない」という薄い失敗になり、何を入れれば直るかが読めなくなるため。
/// env に空文字が入っている場合は「その道具を使わない」という明示なので `None`。
fn find_tool(env_key: &str, candidates: &[&str]) -> Option<OsString> {
    if let Some(named) = env::var_os(env_key) {
        if named.is_empty() {
            return None;
        }
        return which(named.to_str()?).map(OsString::from);
    }
    candidates
        .iter()
        .find_map(|name| which(name))
        .map(OsString::from)
}

/// PATH を自分で引く。build script に依存を足さないための最小実装。
fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let direct = PathBuf::from(name);
        return direct.is_file().then_some(direct);
    }
    env::var_os("PATH")?
        .to_str()?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.is_file())
}

/// 一本焼く。コンパイラの流儀は実行ファイル名から見分ける。
fn compile(compiler: &OsString, source: &str, out: &Path, stage: &str) {
    let is_glslc = Path::new(compiler)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("glslc"));

    let mut command = Command::new(compiler);
    if is_glslc {
        command
            .arg(format!("-fshader-stage={stage}"))
            .arg(format!("--target-env={TARGET_ENV}"));
    } else {
        command.arg("-V").arg("--target-env").arg(TARGET_ENV);
    }
    command.arg("-o").arg(out).arg(source);

    let output = command.output().unwrap_or_else(|e| {
        fail(&format!(
            "{} を起動できない: {e}",
            compiler.to_string_lossy()
        ))
    });
    if !output.status.success() {
        fail(&format!(
            "{source} の SPIR-V 生成が失敗した ({}, 終了 {}).\n{}{}",
            compiler.to_string_lossy(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    if !out.is_file() {
        fail(&format!(
            "{} は成功を返したが {} が無い。",
            compiler.to_string_lossy(),
            out.display()
        ));
    }
}

/// 焼いた物を検証する。ここで落ちるのは生成物が Vulkan に食わせられない場合だけ。
fn validate(validator: &OsString, out: &Path) {
    let output = Command::new(validator)
        .arg("--target-env")
        .arg(TARGET_ENV)
        .arg(out)
        .output()
        .unwrap_or_else(|e| {
            fail(&format!(
                "{} を起動できない: {e}",
                validator.to_string_lossy()
            ))
        });
    if !output.status.success() {
        fail(&format!(
            "{} が不正な SPIR-V だと言っている ({}, 終了 {}).\n{}{}",
            out.display(),
            validator.to_string_lossy(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
}

/// 何が足りないかを、直し方まで含めて書く。
fn missing_compiler_message() -> String {
    let searched = match env::var(COMPILER_ENV) {
        Ok(named) => format!("{COMPILER_ENV}={named} (名指しされたが実在しない)"),
        Err(_) => COMPILER_CANDIDATES.join(" / "),
    };
    format!(
        "GLSL から SPIR-V を焼く道具が見つからない。crates/schorl-render は shader を\n\
         source (shaders/*.vert, shaders/*.frag) で持ち、.spv を repo へ置かないので、\n\
         build には生成器が要る。\n\
         探した名前: {candidates} (PATH: {path})\n\
         直し方のどれか:\n\
           - apt install glslang-tools   (glslangValidator が入る)\n\
           - apt install spirv-tools     (検証に使う spirv-val。任意)\n\
           - {COMPILER_ENV}=/path/to/glslangValidator cargo build",
        candidates = searched,
        path = env::var("PATH").unwrap_or_else(|_| "(未設定)".into()),
    )
}

/// build script の失敗を、cargo が見せる stderr へ書いて落とす。
///
/// panic にしないのは、backtrace の体裁ではなく「何が足りないか」だけを見せたいから。
fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}
