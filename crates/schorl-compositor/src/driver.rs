//! 画面を持たずに compositor を回す駆動部。
//!
//! 入れ子 backend ([`crate::nested`]) は宿主のセッションと GPU を要るが、
//! 「自分のソケットに実在のクライアントが繋がり、`xdg_toplevel` が出て、
//! バッファが載る」という一本は描画なしで確かめられる。ここはそのための
//! 最小の回し方である。
//!
//! 描かないので、クライアントが出した画素は
//! [`crate::buffer::ClientTextureImporter`] へ渡るだけで、どこにも現れない。
//! それが正しい — 画素を絵にするのは別の面の仕事である。

use std::process::{Child, Command};
use std::time::{Duration, Instant};

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::compositor::{
    SurfaceAttributes, TraversalAction, with_surface_tree_downward,
};

use crate::socket::OwnedSocket;
use crate::state::{CompositorClientData, CompositorSetup, SchorlCompositor};

/// 画面なしで回すときの設定。
#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    /// 起動直後に繋がせるクライアント (argv)。空なら何も起こさない。
    pub client: Vec<String>,
    /// これだけ経ったら抜ける。
    pub run_for: Duration,
    /// ウィンドウにバッファが載った時点で抜けるか。
    pub stop_once_mapped: bool,
    /// 一回りごとの待ち。忙しく回さないために要る。
    pub tick: Duration,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            client: Vec::new(),
            run_for: Duration::from_secs(10),
            stop_once_mapped: true,
            tick: Duration::from_millis(4),
        }
    }
}

/// 回し終えたときに分かったこと。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessOutcome {
    /// schorl が取ったソケット名。
    pub socket: String,
    /// 受け入れたクライアント数。
    pub clients_accepted: usize,
    /// 追跡した toplevel の最大数。
    pub toplevels_seen: usize,
    /// バッファが載ったウィンドウの数。
    pub mapped_windows: usize,
    /// 回った回数。
    pub rounds: u64,
}

impl HeadlessOutcome {
    /// 実在のクライアントが繋がり、toplevel が出て、絵が載ったか。
    pub const fn a_client_actually_showed_a_window(&self) -> bool {
        self.clients_accepted > 0 && self.toplevels_seen > 0 && self.mapped_windows > 0
    }
}

/// 画面なしの一回きりの座。
///
/// 起こした display / 状態 / ソケットを一つの scope に束ねて持つ
/// (`house.resource_lifecycle.same_scope`)。drop でソケットファイルが消える。
pub struct HeadlessSession {
    display: Display<SchorlCompositor>,
    handle: smithay::reexports::wayland_server::DisplayHandle,
    state: SchorlCompositor,
    socket: OwnedSocket,
    clients_accepted: usize,
}

impl std::fmt::Debug for HeadlessSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeadlessSession")
            .field("socket", &self.socket.name())
            .field("clients_accepted", &self.clients_accepted)
            .finish_non_exhaustive()
    }
}

impl HeadlessSession {
    /// display を作り、global を出し、自分のソケットを取る。
    pub fn start(setup: CompositorSetup) -> Result<Self> {
        let display: Display<SchorlCompositor> = Display::new().map_err(|e| {
            Error::new(
                ErrorCode::Internal,
                "could not create the wayland display",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })?;
        let handle = display.handle();
        let state = SchorlCompositor::new(&handle, setup)?;
        let socket = OwnedSocket::bind_auto()?;
        let _ = state
            .journal()
            .info(format!("schorl is listening on {}", socket.name()));
        Ok(Self {
            display,
            handle,
            state,
            socket,
            clients_accepted: 0,
        })
    }

    /// schorl が取ったソケット名。
    pub fn socket_name(&self) -> &str {
        self.socket.name().as_str()
    }

    /// ソケットの絶対パス。直に繋ぎに来る面が使う。
    pub fn socket_path(&self) -> Result<std::path::PathBuf> {
        let dir = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "XDG_RUNTIME_DIR is not set, so the socket has no well-known path",
                TraceId::unattributed(),
            )
        })?;
        Ok(std::path::Path::new(&dir).join(self.socket_name()))
    }

    /// compositor の状態。
    pub const fn state(&self) -> &SchorlCompositor {
        &self.state
    }

    /// compositor の状態 (書き換え用)。入力を配る面が使う。
    pub const fn state_mut(&mut self) -> &mut SchorlCompositor {
        &mut self.state
    }

    /// これまでに受け入れたクライアント数。
    pub const fn clients_accepted(&self) -> usize {
        self.clients_accepted
    }

    /// 一回り回す。繋ぎに来たものを入れ、届いた要求を捌き、frame callback を返す。
    pub fn turn(&mut self, time_ms: u32) -> Result<()> {
        while let Some(stream) = self.socket.accept()? {
            if self
                .handle
                .insert_client(stream, std::sync::Arc::new(CompositorClientData::default()))
                .is_ok()
            {
                self.clients_accepted += 1;
            }
        }

        self.display
            .dispatch_clients(&mut self.state)
            .map_err(|e| {
                Error::new(
                    ErrorCode::Internal,
                    "dispatching wayland clients failed",
                    TraceId::unattributed(),
                )
                .caused_by(e)
            })?;

        for window in self.state.windows().iter() {
            send_frames(window.handle().wl_surface(), time_ms);
        }

        self.display.flush_clients().map_err(|e| {
            Error::new(
                ErrorCode::Internal,
                "flushing wayland clients failed",
                TraceId::unattributed(),
            )
            .caused_by(e)
        })
    }

    /// 条件が満たされるまで、最長 `limit` だけ回す。満たされたら `true`。
    pub fn pump_until(
        &mut self,
        limit: Duration,
        tick: Duration,
        ready: impl Fn(&SchorlCompositor) -> bool,
    ) -> Result<bool> {
        let started = Instant::now();
        loop {
            let elapsed = started.elapsed();
            let time_ms = u32::try_from(elapsed.as_millis() % u128::from(u32::MAX)).unwrap_or(0);
            self.turn(time_ms)?;
            if ready(&self.state) {
                return Ok(true);
            }
            if elapsed >= limit {
                return Ok(false);
            }
            std::thread::sleep(tick);
        }
    }
}

/// 画面なしで compositor を回す。
pub fn run_headless(setup: CompositorSetup, options: HeadlessOptions) -> Result<HeadlessOutcome> {
    let mut session = HeadlessSession::start(setup)?;
    let socket_name = session.socket_name().to_owned();
    let mut child = spawn_client(&options.client, &socket_name)?;

    let started = Instant::now();
    let mut rounds: u64 = 0;
    let mut toplevels_seen = 0usize;

    while started.elapsed() < options.run_for {
        rounds += 1;
        let time_ms =
            u32::try_from(started.elapsed().as_millis() % u128::from(u32::MAX)).unwrap_or(0);
        session.turn(time_ms)?;
        toplevels_seen = toplevels_seen.max(session.state().windows().len());
        if options.stop_once_mapped && session.state().windows().iter().any(|w| w.is_mapped()) {
            break;
        }
        std::thread::sleep(options.tick);
    }

    let mapped_windows = session
        .state()
        .windows()
        .iter()
        .filter(|w| w.is_mapped())
        .count();
    let clients_accepted = session.clients_accepted();

    if let Some(child) = child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // session はここで drop され、ソケットファイルと lock ファイルが消える。
    drop(session);

    Ok(HeadlessOutcome {
        socket: socket_name,
        clients_accepted,
        toplevels_seen,
        mapped_windows,
        rounds,
    })
}

fn spawn_client(argv: &[String], socket_name: &str) -> Result<Option<Child>> {
    let Some((program, args)) = argv.split_first() else {
        return Ok(None);
    };
    let child = Command::new(program)
        .args(args)
        // 自分の環境は書き換えず、子にだけ schorl のソケットを見せる。
        .env("WAYLAND_DISPLAY", socket_name)
        .env("XDG_SESSION_TYPE", "wayland")
        .env_remove("DISPLAY")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            Error::new(
                ErrorCode::HostRefused,
                "could not start the verification client",
                TraceId::unattributed(),
            )
            .with_detail("program", program.as_str())
            .caused_by(e)
        })?;
    Ok(Some(child))
}

fn send_frames(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    time_ms: u32,
) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time_ms);
            }
        },
        |_, _, &()| true,
    );
}
