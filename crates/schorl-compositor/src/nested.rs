//! 既存の Wayland セッションの上へ入れ子で起こす検証経路。
//!
//! HMD が無いところで機械が確かめられるのは、`pin verify.machine_scope` の
//! 五点のうち `build_passes` / `input_event_delivered_to_client` /
//! `unit_tests` の側である。この module はそのうち「自分のソケットに
//! 実在のクライアントが繋がり、toplevel が出て、入力が届く」ところを
//! 実機なしで走らせるために在る。
//!
//! **これは受け入れではない。**
//! `pin verify.no_green_substitute: forbid schorl.machine_green =
//! substitute_for_hmd_acceptance` により、ここが緑でも実機受け入れの代わりには
//! ならない。
//!
//! # 宿主を壊さない
//!
//! この経路は宿主の compositor へ一切書き込まない。winit のクライアントとして
//! 窓を一枚もらうだけで、宿主の設定も出力構成も触らない
//! (`pin host.no_persistent_config_change` / `pin wm.host_compositor_coexistence`)。

use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use schorl_core::error::{Error, ErrorCode, Result};

use smithay::backend::input::{
    AbsolutePositionEvent, Event as _, InputEvent, KeyboardKeyEvent, PointerButtonEvent,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{Color32F, Frame as _, Renderer as _};
use smithay::backend::winit::{self, WinitEvent};
use smithay::reexports::wayland_server::Display;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::reexports::winit::window::Window as WinitWindow;
use smithay::utils::{Rectangle, Transform};
use smithay::wayland::compositor::{
    SurfaceAttributes, TraversalAction, with_surface_tree_downward,
};

use schorl_input::{ButtonState, PointerButton};
use schorl_panel::cursor::PixelPoint;

use crate::pointer::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use crate::socket::OwnedSocket;
use crate::state::{CompositorSetup, SchorlCompositor};
use crate::window::AtlasPoint;

/// Ctrl-C / SIGTERM が来たら立つ旗。
///
/// 立つと loop は次の回で普通に抜ける。普通に抜けるからこそ
/// [`OwnedSocket`] の drop が走り、ソケットファイルが消える
/// (`pin host.created_resource_lifecycle`)。
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signum: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    // SAFETY: 旗を立てるだけの handler であり、async-signal-safe。
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

/// 入れ子で起こすときの設定。
#[derive(Debug, Clone)]
pub struct NestedOptions {
    /// 宿主に見せる窓の題。
    pub title: String,
    /// 宿主に見せる窓の大きさ (論理画素)。
    pub window_size: (f64, f64),
    /// atlas をこの窓へ描くときの縮尺。
    pub view_scale: f64,
    /// 起動直後に繋がせるクライアント (argv)。空なら何も起こさない。
    pub client: Vec<String>,
    /// この時間が過ぎたら普通に抜ける。`None` なら窓が閉じるまで回る。
    pub run_for: Option<Duration>,
}

impl Default for NestedOptions {
    fn default() -> Self {
        Self {
            title: "schorl (nested)".to_owned(),
            window_size: (1280.0, 800.0),
            view_scale: 0.5,
            client: Vec::new(),
            run_for: None,
        }
    }
}

/// 回し終えたときに分かったこと。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedOutcome {
    /// schorl が取ったソケット名。
    pub socket: String,
    /// 描いた回数。
    pub frames: u64,
    /// 受け入れたクライアント数。
    pub clients_accepted: usize,
    /// 追跡した toplevel の数 (最大値)。
    pub toplevels_seen: usize,
    /// 最後までバッファが載ったウィンドウの数。
    pub mapped_windows: usize,
}

fn envelope(code: ErrorCode, msg: &str, cause: impl std::fmt::Display) -> Error {
    Error::new(
        code,
        msg.to_owned(),
        schorl_core::id::TraceId::unattributed(),
    )
    .with_detail("reason", cause.to_string())
}

/// 入れ子 backend で compositor を起こし、[`NestedOptions::run_for`] まで回す。
pub fn run(setup: CompositorSetup, options: NestedOptions) -> Result<NestedOutcome> {
    install_signal_handlers();

    let mut display: Display<SchorlCompositor> = Display::new().map_err(|e| {
        envelope(
            ErrorCode::Internal,
            "could not create the wayland display",
            e,
        )
    })?;
    let mut dh = display.handle();

    let mut state = SchorlCompositor::new(&dh, setup)?;
    let socket = OwnedSocket::bind_auto()?;
    let socket_name = socket.name().as_str().to_owned();
    let _ = state
        .journal()
        .info(format!("schorl is listening on {socket_name}"));

    let attributes = WinitWindow::default_attributes()
        .with_inner_size(smithay::reexports::winit::dpi::LogicalSize::new(
            options.window_size.0,
            options.window_size.1,
        ))
        .with_title(options.title.clone())
        .with_visible(true);

    let (mut backend, mut winit_loop) = winit::init_from_attributes::<GlesRenderer>(attributes)
        .map_err(|e| {
            envelope(
                ErrorCode::CapabilityUnavailable,
                "could not open a nested window on the host session",
                e,
            )
        })?;

    let mut child = spawn_client(&options.client, &socket_name, &state)?;

    let started = Instant::now();
    let mut frames: u64 = 0;
    let mut clients_accepted = 0usize;
    let mut toplevels_seen = 0usize;

    while state.is_running() {
        if INTERRUPTED.load(Ordering::SeqCst) {
            let _ = state
                .journal()
                .info("interrupted; letting go of the socket");
            break;
        }
        if options
            .run_for
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            let _ = state
                .journal()
                .info("the nested run reached its time limit");
            break;
        }

        let view_scale = options.view_scale;
        let window_size = backend.window_size();
        let status = winit_loop.dispatch_new_events(|event| match event {
            WinitEvent::CloseRequested => state.stop(),
            WinitEvent::Input(event) => {
                feed_input(&mut state, event, window_size, view_scale);
            }
            _ => {}
        });
        if matches!(status, PumpStatus::Exit(_)) {
            break;
        }

        frames += 1;
        draw_once(&mut backend, &mut state, options.view_scale)?;

        let elapsed_ms =
            u32::try_from(started.elapsed().as_millis() % u128::from(u32::MAX)).unwrap_or(0);
        for window in state.windows().iter() {
            send_frames(window.handle().wl_surface(), elapsed_ms);
        }

        while let Some(stream) = socket.accept()? {
            match dh.insert_client(
                stream,
                std::sync::Arc::new(crate::state::CompositorClientData::default()),
            ) {
                Ok(_client) => {
                    // 取っ手は握らない。クライアントの生存は Display が持っている。
                    clients_accepted += 1;
                    let _ = state
                        .journal()
                        .info(format!("accepted client #{clients_accepted}"));
                }
                Err(e) => {
                    let _ = state
                        .journal()
                        .warn(format!("could not insert a client: {e}"));
                }
            }
        }

        display
            .dispatch_clients(&mut state)
            .map_err(|e| envelope(ErrorCode::Internal, "dispatching wayland clients failed", e))?;
        display
            .flush_clients()
            .map_err(|e| envelope(ErrorCode::Internal, "flushing wayland clients failed", e))?;

        toplevels_seen = toplevels_seen.max(state.windows().len());

        backend
            .submit(None)
            .map_err(|e| envelope(ErrorCode::Internal, "presenting the nested frame failed", e))?;
    }

    let mapped_windows = state.windows().iter().filter(|w| w.is_mapped()).count();

    if let Some(child) = child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }

    // socket はここで drop され、ソケットファイルと lock ファイルが消える。
    drop(socket);

    Ok(NestedOutcome {
        socket: socket_name,
        frames,
        clients_accepted,
        toplevels_seen,
        mapped_windows,
    })
}

fn spawn_client(
    argv: &[String],
    socket_name: &str,
    state: &SchorlCompositor,
) -> Result<Option<Child>> {
    let Some((program, args)) = argv.split_first() else {
        return Ok(None);
    };
    // 自分の環境は書き換えない。子にだけ schorl のソケットを見せる。
    let child = Command::new(program)
        .args(args)
        .env("WAYLAND_DISPLAY", socket_name)
        .env("XDG_SESSION_TYPE", "wayland")
        .env_remove("DISPLAY")
        .spawn()
        .map_err(|e| {
            envelope(
                ErrorCode::HostRefused,
                "could not start the verification client",
                e,
            )
            .with_detail("program", program.as_str())
        })?;
    let _ = state
        .journal()
        .info(format!("started {program} against {socket_name}"));
    Ok(Some(child))
}

fn draw_once(
    backend: &mut winit::WinitGraphicsBackend<GlesRenderer>,
    state: &mut SchorlCompositor,
    view_scale: f64,
) -> Result<()> {
    let size = backend.window_size();
    let damage = Rectangle::from_size(size);

    let (renderer, mut framebuffer) = backend.bind().map_err(|e| {
        envelope(
            ErrorCode::Internal,
            "could not bind the nested framebuffer",
            e,
        )
    })?;

    let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
    for window in state.windows().iter() {
        let rect = window.rect();
        let location = (
            (f64::from(rect.x) * view_scale) as i32,
            (f64::from(rect.y) * view_scale) as i32,
        );
        elements.extend(render_elements_from_surface_tree(
            renderer,
            window.handle().wl_surface(),
            location,
            view_scale,
            1.0,
            Kind::Unspecified,
        ));
    }

    let mut frame = renderer
        .render(&mut framebuffer, size, Transform::Flipped180)
        .map_err(|e| envelope(ErrorCode::Internal, "starting the nested frame failed", e))?;
    // `pin space.background: require schorl.virtual_space.background = black`
    frame
        .clear(Color32F::new(0.0, 0.0, 0.0, 1.0), &[damage])
        .map_err(|e| envelope(ErrorCode::Internal, "clearing the nested frame failed", e))?;
    draw_render_elements(&mut frame, view_scale, &elements, &[damage])
        .map_err(|e| envelope(ErrorCode::Internal, "drawing the nested frame failed", e))?;
    let _sync = frame
        .finish()
        .map_err(|e| envelope(ErrorCode::Internal, "finishing the nested frame failed", e))?;
    Ok(())
}

const fn millis(time_msec: u32) -> u32 {
    time_msec
}

fn feed_input(
    state: &mut SchorlCompositor,
    event: InputEvent<winit::WinitInput>,
    window_size: smithay::utils::Size<i32, smithay::utils::Physical>,
    view_scale: f64,
) {
    match event {
        InputEvent::PointerMotionAbsolute { event } => {
            let time_ms = millis(event.time_msec());
            let x = event.x_transformed(window_size.w) / view_scale;
            let y = event.y_transformed(window_size.h) / view_scale;
            let point = AtlasPoint { x, y };
            let target = state
                .windows()
                .window_at(point)
                .map(|w| (w.id().clone(), w.rect()));
            if let Some((id, rect)) = target {
                let local = PixelPoint {
                    x: (x - f64::from(rect.x)) as i32,
                    y: (y - f64::from(rect.y)) as i32,
                };
                let _ = state.point_at_local(&id, local, time_ms);
            }
        }
        InputEvent::PointerButton { event } => {
            let time_ms = millis(event.time_msec());
            let button = match event.button_code() {
                BTN_LEFT => Some(PointerButton::Left),
                BTN_RIGHT => Some(PointerButton::Right),
                BTN_MIDDLE => Some(PointerButton::Middle),
                _ => None,
            };
            if let Some(button) = button {
                let pressed = match event.state() {
                    smithay::backend::input::ButtonState::Pressed => ButtonState::Pressed,
                    smithay::backend::input::ButtonState::Released => ButtonState::Released,
                };
                let _ = state.press_button(button, pressed, time_ms);
            }
        }
        InputEvent::Keyboard { event } => {
            let time_ms = millis(event.time_msec());
            // winit backend が返すのは xkb の番号なので、evdev へ戻してから渡す。
            let evdev = event
                .key_code()
                .raw()
                .saturating_sub(crate::keyboard::EVDEV_TO_XKB_OFFSET);
            let pressed = match event.state() {
                smithay::backend::input::KeyState::Pressed => ButtonState::Pressed,
                smithay::backend::input::KeyState::Released => ButtonState::Released,
            };
            let _ = state.press_key(schorl_input::Keycode(evdev), pressed, time_ms);
        }
        _ => {}
    }
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
