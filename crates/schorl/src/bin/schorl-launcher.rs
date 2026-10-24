//! schorl の通常の Wayland client として動く VR application launcher。

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use schorl::launcher::{LauncherEntry, discover, entry_at, paint};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WIDTH: i32 = 1280;
const HEIGHT: i32 = 720;

struct Client {
    entries: Vec<LauncherEntry>,
    pixels: Vec<u8>,
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    committed: bool,
    pointer: (f64, f64),
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
            if !state.committed {
                state.surface.attach(Some(&state.buffer), 0, 0);
                state.surface.damage(0, 0, WIDTH, HEIGHT);
                state.surface.commit();
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
impl Dispatch<wl_seat::WlSeat, ()> for Client {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.pointer = (surface_x, surface_y),
            wl_pointer::Event::Button {
                button: 0x110,
                state: WEnum::Value(wl_pointer::ButtonState::Released),
                ..
            } => {
                if let Some(index) = entry_at(state.pointer.0, state.pointer.1, state.entries.len())
                    && let Err(error) = state.entries[index].spawn()
                {
                    eprintln!(
                        "schorl-launcher could not start {}: {error}",
                        state.entries[index].name
                    );
                }
            }
            _ => {}
        }
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
            eprintln!("schorl-launcher failed: {reason}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let entries = discover();
    let pixels = paint(&entries, WIDTH as u32, HEIGHT as u32);
    let display =
        std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY is not set".to_owned())?;
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
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=7, ())
        .map_err(|e| format!("wl_seat: {e}"))?;

    let stride = WIDTH * 4;
    let size = (stride * HEIGHT) as usize;
    let file = scratch_file(&pixels, &socket)?;
    let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&file), size as i32, &qh, ());
    let buffer = pool.create_buffer(0, WIDTH, HEIGHT, stride, wl_shm::Format::Xrgb8888, &qh, ());
    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("schorl launcher".to_owned());
    toplevel.set_app_id("schorl-launcher".to_owned());
    surface.commit();

    let mut state = Client {
        entries,
        pixels,
        surface,
        buffer,
        committed: false,
        pointer: (0.0, 0.0),
    };
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("first roundtrip: {e}"))?;
    let pointer = seat.get_pointer(&qh, ());
    loop {
        if queue.roundtrip(&mut state).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    pointer.release();
    let _ = state.pixels.len();
    let _ = xdg;
    let _ = toplevel;
    Ok(())
}

fn scratch_file(pixels: &[u8], near: &std::path::Path) -> Result<std::fs::File, String> {
    let dir = near.parent().unwrap_or(std::path::Path::new("/tmp"));
    let path = dir.join(format!("schorl-launcher-{}", std::process::id()));
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
