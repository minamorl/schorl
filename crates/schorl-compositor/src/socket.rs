//! schorl 自身の Wayland ソケット。
//!
//! `pin wm.own_socket: require schorl.wayland.socket = owned_by_schorl`
//! — クライアントは宿主の compositor ではなく、ここで作った名前へ繋ぐ。
//!
//! `pin wm.host_compositor_coexistence: forbid schorl.host_compositor.session = replaced`
//! — 宿主が使っている名前は取らない。[`OwnedSocket::bind_auto`] は空いている
//!   名前を自分で選び、[`OwnedSocket::bind_named`] は宿主の
//!   `WAYLAND_DISPLAY` と同じ名前を拒む。
//!
//! `pin host.created_resource_lifecycle: require schorl.created_host_resource.lifetime
//!  = released_at_process_exit`
//! — ソケットファイルと lock ファイルは `wayland_server::ListeningSocket` の drop が
//!   `remove_file` で消す (wayland-server 0.31 の `impl Drop for ListeningSocket`)。
//!   この型はその drop を握ったまま持ち回るだけで、勝手に忘れない。
//!   `house.resource_lifecycle.explicit_escape` に従い、外へ渡すのは
//!   [`OwnedSocket::into_inner`] という明示の一本だけにしてある。

use std::ffi::OsStr;

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use std::os::unix::net::UnixStream;

use smithay::reexports::wayland_server::ListeningSocket;

/// ソケットの名前。`WAYLAND_DISPLAY` に入る値そのもの。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SocketName(String);

impl SocketName {
    /// 名前から作る。空は拒む。
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "a wayland socket name must not be empty",
                TraceId::unattributed(),
            ));
        }
        Ok(Self(name))
    }

    /// `WAYLAND_DISPLAY` へ入れる値。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SocketName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// schorl が所有する待ち受けソケット。
///
/// 所有権を外へ出す口は [`Self::into_inner`] だけ。それ以外の経路で
/// 中身が逃げないので、drop まで所有が同じ scope に居る。
#[derive(Debug)]
pub struct OwnedSocket {
    socket: ListeningSocket,
    name: SocketName,
}

impl OwnedSocket {
    /// 空いている `wayland-N` を自分で選んで待ち受ける。
    ///
    /// 宿主が使っている名前は既に lock されているので選ばれない。
    pub fn bind_auto() -> Result<Self> {
        // wayland-0 は避ける。既存のクライアントが取り違えないようにするため。
        let socket =
            ListeningSocket::bind_auto("wayland", 1..33).map_err(|e| bind_failed("auto", &e))?;
        let name = socket_name_of(&socket)?;
        Ok(Self { socket, name })
    }

    /// 名前を指定して待ち受ける。
    ///
    /// 宿主の `WAYLAND_DISPLAY` と同じ名前は、取れてしまう前に拒む。
    pub fn bind_named(name: &str) -> Result<Self> {
        if std::env::var("WAYLAND_DISPLAY").is_ok_and(|host| host == name) {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "schorl refuses to take over the host compositor's socket name",
                TraceId::unattributed(),
            )
            .with_detail("name", name));
        }
        let socket = ListeningSocket::bind(name).map_err(|e| bind_failed(name, &e))?;
        let name = socket_name_of(&socket)?;
        Ok(Self { socket, name })
    }

    /// 取れた名前。
    pub const fn name(&self) -> &SocketName {
        &self.name
    }

    /// 繋ぎに来たクライアントがあれば、その stream を返す。
    ///
    /// 誰も来ていなければ `Ok(None)`。
    pub fn accept(&self) -> Result<Option<UnixStream>> {
        self.socket.accept().map_err(|e| {
            Error::new(
                ErrorCode::HostRefused,
                "could not accept a client on schorl's socket",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })
    }

    /// 中身の所有権を渡す。**明示の持ち出しはこの一本だけ**
    /// (`house.resource_lifecycle.explicit_escape`)。
    pub fn into_inner(self) -> ListeningSocket {
        self.socket
    }
}

fn socket_name_of(socket: &ListeningSocket) -> Result<SocketName> {
    let raw: &OsStr = socket.socket_name().ok_or_else(|| {
        Error::new(
            ErrorCode::Internal,
            "the bound socket reports no name",
            TraceId::unattributed(),
        )
    })?;
    let text = raw.to_str().ok_or_else(|| {
        Error::new(
            ErrorCode::Internal,
            "the socket name is not valid UTF-8",
            TraceId::unattributed(),
        )
    })?;
    SocketName::new(text)
}

fn bind_failed(name: &str, cause: &smithay::reexports::wayland_server::BindError) -> Error {
    Error::new(
        ErrorCode::HostRefused,
        "could not bind schorl's own wayland socket",
        TraceId::unattributed(),
    )
    .with_detail("name", name)
    .with_detail("reason", format!("{cause}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_socket_name_is_refused_with_an_envelope() {
        let err = SocketName::new("").expect_err("empty is refused");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn a_bound_socket_reports_a_name_and_removes_its_files_when_dropped() {
        // 前提の門。runtime dir が無いなら何も測れないが、**測れなかった走りを
        // `ok` と数えない。** 何が無いかを名指しして落ちる。
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
            panic!(
                "XDG_RUNTIME_DIR is not set, so schorl has nowhere to place its own wayland \
                 socket and this check would measure nothing. run it inside a user session, \
                 or point XDG_RUNTIME_DIR at a writable directory."
            )
        });
        let socket = OwnedSocket::bind_auto().expect("a free wayland-N exists");
        let name = socket.name().as_str().to_owned();
        let path = std::path::Path::new(&runtime_dir).join(&name);
        assert!(path.exists(), "{path:?} should exist while schorl holds it");
        drop(socket);
        assert!(!path.exists(), "{path:?} must be gone once schorl lets go");
    }

    // 宿主の compositor が実際に一本ソケットを持っている場所でしか測れない。
    // 黙って `return` して `ok` を数える形を捨て、前提が無いことを `ignore` の
    // 理由として表に出す (走らせ方: `cargo test -p schorl-compositor -- --ignored`)。
    #[test]
    #[ignore = "needs WAYLAND_DISPLAY naming a live host compositor socket; run with --ignored \
                inside that session"]
    fn the_host_compositor_socket_name_is_refused() {
        let host = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| {
            panic!(
                "WAYLAND_DISPLAY is not set, so there is no host socket name to refuse and \
                 this check would measure nothing. run it inside a wayland session."
            )
        });
        let err = OwnedSocket::bind_named(&host).expect_err("the host keeps its socket");
        assert_eq!(err.code(), ErrorCode::InvalidArgument);
    }
}
