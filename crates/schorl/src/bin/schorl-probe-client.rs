//! schorl のソケットへ繋いで、**自分で決めた**四分割の絵を一枚出すだけの
//! Wayland クライアント。
//!
//! # なぜ絵をクライアントに決めさせるのか
//!
//! `pin verify.machine_scope` の `client_frame_reaches_swapchain` は、
//! クライアントの画素が swapchain まで届くことを言っている。届いた絵が
//! こちら (compositor / 描画側) の作った絵と区別できなければ、この項目は
//! 測れていない。
//!
//! そこでこのクライアントは、起動のたびに 7 色から 4 色を選んで並べ、
//! その並びを **描く前に** 標準出力へ一行で申告する:
//!
//! ```text
//! schorl-probe-client pattern=RGBW
//! ```
//!
//! 検証側は申告を読み、swapchain から読み戻した絵に同じ並びが出るかを見る。
//! 並びは 840 通りあるので、描画側が中身を知らずに当てることはできない。
//!
//! **これは受け入れではない** (`pin verify.no_green_substitute`)。

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use schorl::probe::{PureColour, QuadPattern, paint_quadrants};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// 絵の横幅 (画素)。toplevel へ送られる既定と同じ。
const WIDTH: i32 = 1280;
/// 絵の高さ (画素)。
const HEIGHT: i32 = 720;

struct Client {
    pixels: Vec<u8>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    committed: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let (false, Some(wl), Some(buffer)) = (state.committed, &state.surface, &state.buffer)
            {
                wl.attach(Some(buffer), 0, 0);
                wl.damage(0, 0, WIDTH, HEIGHT);
                wl.commit();
                state.committed = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Client {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("schorl-probe-client failed: {reason}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    // 並びは引数で指定できる (再現のため)。無ければ自分で選ぶ。
    let pattern = match std::env::args().nth(1) {
        Some(letters) => QuadPattern::from_letters(&letters)
            .ok_or_else(|| format!("{letters:?} is not four distinct colour letters"))?,
        None => choose_pattern(),
    };
    // **描く前に申告する。** あとから言い換えられないことがこの一行の意味である。
    println!("schorl-probe-client pattern={}", pattern.to_letters());
    std::io::stdout().flush().map_err(|e| format!("flush: {e}"))?;

    let display = std::env::var("WAYLAND_DISPLAY")
        .map_err(|_| "WAYLAND_DISPLAY is not set".to_owned())?;
    let runtime =
        std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set".to_owned())?;
    let socket = std::path::Path::new(&runtime).join(&display);
    let stream = UnixStream::connect(&socket).map_err(|e| format!("connect {socket:?}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("from_socket: {e}"))?;

    let (globals, mut queue) =
        registry_queue_init::<Client>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();

    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .map_err(|e| format!("wl_compositor: {e}"))?;
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wl_shm: {e}"))?;
    let wm_base: xdg_wm_base::XdgWmBase = globals
        .bind(&qh, 1..=3, ())
        .map_err(|e| format!("xdg_wm_base: {e}"))?;

    let stride = WIDTH * 4;
    let size = (stride * HEIGHT) as usize;
    let pixels = paint_quadrants(pattern, WIDTH as u32, HEIGHT as u32);
    if pixels.len() != size {
        return Err(format!("painted {} bytes, wanted {size}", pixels.len()));
    }
    let file = scratch_file(&pixels, &socket)?;
    let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&file), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        WIDTH,
        HEIGHT,
        stride,
        wl_shm::Format::Xrgb8888,
        &qh,
        (),
    );

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title(format!("schorl probe {}", pattern.to_letters()));
    toplevel.set_app_id("schorl-probe-client".to_owned());
    surface.commit();

    let mut state = Client {
        pixels,
        surface: Some(surface.clone()),
        buffer: Some(buffer.clone()),
        committed: false,
    };

    // 親が止めるまで回す。絵は一枚きりで、動かさない。
    let deadline = Instant::now()
        + std::env::var("SCHORL_PROBE_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(Duration::from_secs(120), Duration::from_secs);
    while Instant::now() < deadline {
        if queue.roundtrip(&mut state).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = state.pixels.len();
    let _ = xdg;
    let _ = toplevel;
    Ok(())
}

/// 画素を書き込んだ無名の場所を一つ作る。
///
/// 名前はすぐ外すので、プロセスが落ちても残骸が出ない
/// (`host.created_resource_lifecycle`)。
fn scratch_file(pixels: &[u8], near: &std::path::Path) -> Result<std::fs::File, String> {
    let dir = near.parent().unwrap_or(std::path::Path::new("/tmp"));
    let path = dir.join(format!("schorl-probe-client-{}", std::process::id()));
    let mut file = std::fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("open {path:?}: {e}"))?;
    file.write_all(pixels)
        .map_err(|e| format!("fill {path:?}: {e}"))?;
    file.flush().map_err(|e| format!("flush {path:?}: {e}"))?;
    let _ = std::fs::remove_file(&path);
    Ok(file)
}

/// 7 色から 4 色を選んで並べる。
///
/// 種は時刻の下位桁とプロセス番号から引く。**秘密ではない**ので、乱数の
/// capability をこのバイナリへ持ち込まない (`house.effect_boundary.exceptions`
/// の小さな葉)。
fn choose_pattern() -> QuadPattern {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());
    let mut state = (nanos as u64) ^ (u64::from(std::process::id()) << 32) ^ 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut pool: Vec<PureColour> = PureColour::ALL.to_vec();
    let mut chosen = [PureColour::Red; 4];
    for slot in &mut chosen {
        let index = (next() as usize) % pool.len();
        *slot = pool.remove(index);
    }
    QuadPattern { quadrants: chosen }
}
