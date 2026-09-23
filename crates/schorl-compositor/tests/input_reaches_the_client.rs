//! 入力がクライアントへ本当に届いたかを、届いた側に言わせて測る。
//!
//! `pin v1.click_delivered: require schorl.v1.click = delivered_to_window_client`
//! `pin v1.key_delivered:   require schorl.v1.key_input = delivered_to_window_client`
//! `pin verify.machine_scope` の `input_event_delivered_to_client`
//!
//! schorl 側の状態を読んで「配った」と言うのでは、自分の実装で自分を検査する
//! ことになる。この試験は本物の Wayland クライアントを一本立て、
//! `wl_pointer` / `wl_keyboard` が実際に受け取った引数を読む。
//!
//! 座標が surface-local であることは、クライアントが受け取った
//! `surface_x` / `surface_y` がこちらの渡したウィンドウ内画素と一致することで
//! 示す (`pin ux.cursor_mapping` の下流)。
//!
//! **これは受け入れではない** (`pin verify.no_green_substitute`)。

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use schorl_compositor::PointerDelivery;
use schorl_compositor::driver::HeadlessSession;
use schorl_compositor::journal::{Journal, StderrJsonLogSink, Uuidv7IdGen};
use schorl_compositor::state::CompositorSetup;
use schorl_core::time::SystemClock;
use schorl_input::{ButtonState, Keycode, PointerButton};
use schorl_panel::cursor::{PixelPoint, PointerHold};
use schorl_panel::math::Vec3;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// `linux/input-event-codes.h` の `KEY_A`。
const KEY_A: u32 = 30;
/// `linux/input-event-codes.h` の `BTN_LEFT`。
const BTN_LEFT: u32 = 0x110;

const WINDOW_W: i32 = 1280;
const WINDOW_H: i32 = 720;

/// クライアントが受け取った物。
#[derive(Debug, Default, Clone)]
struct Received {
    pointer_enter: Vec<(f64, f64)>,
    pointer_motion: Vec<(f64, f64)>,
    pointer_buttons: Vec<(u32, u32)>,
    pointer_leaves: usize,
    keys: Vec<(u32, u32)>,
    keymap_arrived: bool,
    configured: bool,
}

struct Client {
    received: Arc<Mutex<Received>>,
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
            if let Ok(mut got) = state.received.lock() {
                got.configured = true;
            }
            // configure に応えてから初めて絵を載せる。
            if let (false, Some(wl), Some(buffer)) =
                (state.committed, &state.surface, &state.buffer)
            {
                wl.attach(Some(buffer), 0, 0);
                wl.damage(0, 0, WINDOW_W, WINDOW_H);
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
        let Ok(mut got) = state.received.lock() else {
            return;
        };
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => got.pointer_enter.push((surface_x, surface_y)),
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => got.pointer_motion.push((surface_x, surface_y)),
            wl_pointer::Event::Leave { .. } => got.pointer_leaves += 1,
            wl_pointer::Event::Button { button, state, .. } => {
                let code = match state {
                    WEnum::Value(wl_pointer::ButtonState::Pressed) => 1,
                    _ => 0,
                };
                got.pointer_buttons.push((button, code));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Ok(mut got) = state.received.lock() else {
            return;
        };
        match event {
            wl_keyboard::Event::Keymap { .. } => got.keymap_arrived = true,
            wl_keyboard::Event::Key { key, state, .. } => {
                let code = match state {
                    WEnum::Value(wl_keyboard::KeyState::Pressed) => 1,
                    _ => 0,
                };
                got.keys.push((key, code));
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

/// クライアント側を一本走らせる。止まれと言われるまで回す。
fn run_client(
    socket: std::path::PathBuf,
    received: Arc<Mutex<Received>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
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

    let stride = WINDOW_W * 4;
    let size = (stride * WINDOW_H) as usize;
    let file = scratch_file(size, &socket)?;
    let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&file), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        WINDOW_W,
        WINDOW_H,
        stride,
        wl_shm::Format::Xrgb8888,
        &qh,
        (),
    );

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("schorl input probe".to_owned());
    surface.commit();

    let mut state = Client {
        received,
        surface: Some(surface.clone()),
        buffer: Some(buffer.clone()),
        committed: false,
    };

    // 先に seat の capability を取ってから、ポインタとキーボードを作る。
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("first roundtrip: {e}"))?;
    let pointer = seat.get_pointer(&qh, ());
    let keyboard = seat.get_keyboard(&qh, ());

    while !stop.load(Ordering::SeqCst) {
        if queue.roundtrip(&mut state).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(3));
    }

    pointer.release();
    keyboard.release();
    let _ = xdg;
    let _ = toplevel;
    Ok(())
}

/// pool 用の場所を一つ作り、名前をすぐ外す。fd だけが残る。
fn scratch_file(size: usize, near: &std::path::Path) -> Result<std::fs::File, String> {
    let dir = near.parent().unwrap_or(std::path::Path::new("/tmp"));
    let path = dir.join(format!("schorl-probe-{}", std::process::id()));
    let mut file = std::fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("open {path:?}: {e}"))?;
    file.write_all(&vec![0u8; size])
        .map_err(|e| format!("fill {path:?}: {e}"))?;
    // 名前を外しても fd は生きる。プロセスが落ちても残骸が出ない。
    let _ = std::fs::remove_file(&path);
    Ok(file)
}

fn setup() -> CompositorSetup {
    let ids = Arc::new(Uuidv7IdGen);
    let journal = Journal::with_new_trace(
        Arc::new(SystemClock),
        Arc::new(StderrJsonLogSink),
        ids.as_ref(),
    )
    .expect("a trace id can be issued");
    CompositorSetup::new(ids, journal)
}

/// 前提の門。**欠けていたら緑を返さず、何が無いかを名指しして落ちる。**
///
/// 以前はここで黙って `return` していた。schorl のソケットを置く場所が無い機械では
/// 何も測れないが、何も測れなかった走りを `ok` として数えると緑の意味が薄まる。
fn require_runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        panic!(
            "XDG_RUNTIME_DIR is not set, so schorl cannot place its own wayland socket \
             and this check would measure nothing. run it inside a user session, or point \
             XDG_RUNTIME_DIR at a writable directory."
        )
    })
}

#[test]
fn a_click_and_a_key_reach_the_client_with_surface_local_coordinates() {
    let _runtime_dir = require_runtime_dir();

    let mut session = HeadlessSession::start(setup()).expect("schorl starts");
    let path = session.socket_path().expect("the socket has a path");
    let received = Arc::new(Mutex::new(Received::default()));
    let stop = Arc::new(AtomicBool::new(false));

    let client_received = Arc::clone(&received);
    let client_stop = Arc::clone(&stop);
    let client = std::thread::spawn(move || run_client(path, client_received, client_stop));

    let mapped = session
        .pump_until(Duration::from_secs(15), Duration::from_millis(2), |state| {
            state.windows().iter().any(|w| w.is_mapped())
        })
        .expect("the compositor runs");
    assert!(mapped, "the probe client should have mapped a toplevel");

    let window = session
        .state()
        .windows()
        .iter()
        .next()
        .expect("one window")
        .id()
        .clone();

    session
        .state_mut()
        .focus_keyboard_on(Some(&window))
        .expect("focus can be set");

    // 一点目は enter を、二点目は motion を生む。
    session
        .state_mut()
        .point_at_local(&window, PixelPoint { x: 37, y: 53 }, 10)
        .expect("pointer is delivered");
    session.turn(11).expect("a turn");
    session
        .state_mut()
        .point_at_local(&window, PixelPoint { x: 240, y: 180 }, 12)
        .expect("pointer is delivered");
    session
        .state_mut()
        .press_button(PointerButton::Left, ButtonState::Pressed, 13)
        .expect("button is delivered");
    session
        .state_mut()
        .press_button(PointerButton::Left, ButtonState::Released, 14)
        .expect("button is delivered");
    session
        .state_mut()
        .press_key(Keycode(KEY_A), ButtonState::Pressed, 15)
        .expect("key is delivered");
    session
        .state_mut()
        .press_key(Keycode(KEY_A), ButtonState::Released, 16)
        .expect("key is delivered");

    let seen = session
        .pump_until(Duration::from_secs(5), Duration::from_millis(2), |_| {
            let got = received.lock().expect("not poisoned");
            !got.pointer_motion.is_empty()
                && !got.pointer_buttons.is_empty()
                && !got.keys.is_empty()
        })
        .expect("the compositor runs");

    stop.store(true, Ordering::SeqCst);
    let got = received.lock().expect("not poisoned").clone();
    drop(session);
    let _ = client.join();

    eprintln!("client received: {got:?}");
    assert!(
        seen,
        "the client should have received pointer and key events: {got:?}"
    );

    assert_eq!(
        got.pointer_enter.first().copied(),
        Some((37.0, 53.0)),
        "wl_pointer.enter must carry surface-local pixels: {got:?}"
    );
    assert!(
        got.pointer_motion.contains(&(240.0, 180.0)),
        "wl_pointer.motion must carry surface-local pixels: {got:?}"
    );
    assert!(
        got.pointer_buttons.contains(&(BTN_LEFT, 1)),
        "the click must reach the client: {got:?}"
    );
    assert!(
        got.pointer_buttons.contains(&(BTN_LEFT, 0)),
        "the release must reach the client too: {got:?}"
    );
    assert!(
        got.keys.contains(&(KEY_A, 1)),
        "wl_keyboard.key must carry the evdev code: {got:?}"
    );
    assert!(got.keymap_arrived, "the client needs a keymap: {got:?}");
}

#[test]
fn a_controller_ray_projected_onto_the_window_plane_lands_on_the_right_pixel() {
    let _runtime_dir = require_runtime_dir();

    let mut session = HeadlessSession::start(setup()).expect("schorl starts");
    let path = session.socket_path().expect("the socket has a path");
    let received = Arc::new(Mutex::new(Received::default()));
    let stop = Arc::new(AtomicBool::new(false));

    let client_received = Arc::clone(&received);
    let client_stop = Arc::clone(&stop);
    let client = std::thread::spawn(move || run_client(path, client_received, client_stop));

    let mapped = session
        .pump_until(Duration::from_secs(15), Duration::from_millis(2), |state| {
            state.windows().iter().any(|w| w.is_mapped())
        })
        .expect("the compositor runs");
    assert!(mapped, "the probe client should have mapped a toplevel");

    let (window, centre) = {
        let w = session.state().windows().iter().next().expect("one window");
        (w.id().clone(), w.plane().pose().pose.position)
    };

    // 板は 1280x720 画素を 1600 px/m で立てているので 0.8m x 0.45m。
    // 中心から右へ 0.10m、上へ 0.05m の点は
    //   x = (0.10 + 0.40) / 0.80 * 1280 = 800
    //   y = (0.225 - 0.05) / 0.45 * 720 = 280
    // に落ちる。この数はここで手で出したものであって、実装から借りていない。
    let aim = Vec3::new(centre.x + 0.10, centre.y + 0.05, centre.z);
    let delivery = session
        .state_mut()
        .aim_at(&window, aim, PointerHold::Up, 20)
        .expect("the ray resolves");
    match delivery {
        PointerDelivery::Moved { local, .. } => {
            assert!((local.x - 800).abs() <= 1, "x was {local:?}");
            assert!((local.y - 280).abs() <= 1, "y was {local:?}");
        }
        other => panic!("expected the ray to land on the window, got {other:?}"),
    }

    // 縁の外でも、押したままなら追い続ける (`pin ux.cursor_beyond_edge`)。
    let far = Vec3::new(centre.x + 0.60, centre.y, centre.z);
    let held = session
        .state_mut()
        .aim_at(&window, far, PointerHold::Down, 21)
        .expect("the ray resolves");
    match held {
        PointerDelivery::Moved { local, .. } => {
            assert!(
                local.x > 1280,
                "a held cursor keeps going past the edge: {local:?}"
            );
        }
        other => panic!("a held cursor must keep following, got {other:?}"),
    }

    // 離していれば、縁の外では動かさず leave になる。
    let loose = session
        .state_mut()
        .aim_at(&window, far, PointerHold::Up, 22)
        .expect("the ray resolves");
    assert_eq!(loose, PointerDelivery::Left);

    let seen = session
        .pump_until(Duration::from_secs(5), Duration::from_millis(2), |_| {
            let got = received.lock().expect("not poisoned");
            got.pointer_leaves > 0 && !got.pointer_motion.is_empty()
        })
        .expect("the compositor runs");

    stop.store(true, Ordering::SeqCst);
    let got = received.lock().expect("not poisoned").clone();
    drop(session);
    let _ = client.join();

    eprintln!("client received: {got:?}");
    assert!(
        seen,
        "the client should have seen the ray move and then leave: {got:?}"
    );
    assert_eq!(
        got.pointer_enter
            .first()
            .copied()
            .map(|(x, y)| (x.round(), y.round())),
        Some((800.0, 280.0)),
        "the projected ray must arrive as surface-local pixels: {got:?}"
    );
    assert!(
        got.pointer_motion.iter().any(|(x, _)| *x > 1280.0),
        "the held cursor past the edge must still arrive: {got:?}"
    );
    assert!(
        got.pointer_leaves > 0,
        "releasing past the edge must leave: {got:?}"
    );
}
