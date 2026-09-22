//! 秘密。
//!
//! 満たす pin:
//! - `code.secret.source: require secret.source = env_or_vault`
//! - `code.secret.no_commit: forbid secret.location = repo`
//! - `code.log.no_secrets` (この型の `Debug`/`Display` が綴りを出さない)
//!
//! この repo には秘密の値を一つも書かない。取得口は [`SecretSource`] だけで、
//! 実装は環境変数 ([`EnvSecretSource`]) と、後続 phase が足す vault の二系統に限る。

use crate::error::{Error, ErrorCode, Result};
use crate::id::TraceId;
use std::fmt;

/// 中身を印字しない包み。
///
/// `Debug` も `Display` も伏字を出す。取り出しは [`Secret::expose`] だけで、
/// 名前が目立つので読む側が気づける。
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// 包む。
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// 中身を取り出す。呼ぶ場所はログの外に限ること。
    pub const fn expose(&self) -> &T {
        &self.0
    }

    /// 所有権ごと取り出す。
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// 秘密の取得口。
///
/// 許された出所は env と vault の二つ。repo 内のファイルを読む実装は書かない。
pub trait SecretSource: Send + Sync {
    /// 名前で引く。無ければ封筒で返す (名前は出すが値は出さない)。
    fn get(&self, name: &str) -> Result<Secret<String>>;
}

/// 環境変数から引く adapter。
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvSecretSource;

impl SecretSource for EnvSecretSource {
    fn get(&self, name: &str) -> Result<Secret<String>> {
        match std::env::var(name) {
            Ok(value) => Ok(Secret::new(value)),
            Err(_) => Err(Error::new(
                ErrorCode::CapabilityUnavailable,
                "secret is not present in the environment",
                TraceId::unattributed(),
            )
            .with_detail("name", name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_redact_the_value() {
        let s = Secret::new("hunter2".to_owned());
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{s}"), "<redacted>");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn missing_environment_secret_reports_the_name_only() {
        let err = EnvSecretSource
            .get("SCHORL_TEST_SECRET_THAT_DOES_NOT_EXIST")
            .expect_err("unset variable must fail");
        assert_eq!(err.code(), ErrorCode::CapabilityUnavailable);
        assert!(err.details().iter().any(|(k, _)| k == "name"));
    }
}
