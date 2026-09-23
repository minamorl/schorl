//! dmabuf の受け口が本当に開いているかを、クライアント側から測る。
//!
//! この crate は dmabuf を GPU テクスチャへ取り込まない。取り込むのは別の面で、
//! ここが持つのは「受け取って引き渡す口」だけである。だから測るのも口までで、
//! 測り方は
//!
//! 1. クライアントが `zwp_linux_dmabuf_v1` を bind できる
//! 2. `zwp_linux_buffer_params_v1.create` に `created` が返る
//! 3. compositor 側の引き渡し先が、その素性 (fourcc / modifier / 寸法 / plane 数)
//!    を受け取っている
//!
//! の三点である。
//!
//! # 渡している fd について
//!
//! この試験が `add` へ渡すのは、GPU が作った本物の dma_buf ではなく、寸法を
//! 合わせた普通のファイルの fd である。protocol の上では両者は区別されない
//! (smithay は fd の大きさで offset と stride を検査するだけで、`dma_buf` かは
//! 見ない)。**したがってこの試験が示すのは受け口であって、GPU への取り込みが
//! 成り立つことではない。** そちらは取り込む面が測る。

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use schorl_compositor::buffer::{ClientBufferKind, RecordingImporter};
use schorl_compositor::driver::HeadlessSession;
use schorl_compositor::journal::{Journal, StderrJsonLogSink, Uuidv7IdGen};
use schorl_compositor::state::CompositorSetup;
use schorl_core::time::SystemClock;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

/// `DRM_FORMAT_XRGB8888`。fourcc `'X' 'R' '2' '4'`。
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
/// `DRM_FORMAT_MOD_LINEAR`。
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

const W: i32 = 64;
const H: i32 = 32;

#[derive(Debug, Default, Clone)]
struct Outcome {
    created: bool,
    failed: bool,
    bound_dmabuf: bool,
}

struct Client {
    outcome: Arc<Mutex<Outcome>>,
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

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Ok(mut outcome) = state.outcome.lock() else {
            return;
        };
        match event {
            zwp_linux_buffer_params_v1::Event::Created { .. } => outcome.created = true,
            zwp_linux_buffer_params_v1::Event::Failed => outcome.failed = true,
            _ => {}
        }
    }

    // `created` は新しい `wl_buffer` を connection 上に生む。
    // どの user data を付けるかは client 側が決めるので、その指定が要る。
    wayland_client::event_created_child!(Client, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ()),
    ]);
}

delegate_noop!(Client: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);

fn run_client(
    socket: std::path::PathBuf,
    outcome: Arc<Mutex<Outcome>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let stream = UnixStream::connect(&socket).map_err(|e| format!("connect {socket:?}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("from_socket: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<Client>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();

    let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 = globals
        .bind(&qh, 3..=3, ())
        .map_err(|e| format!("zwp_linux_dmabuf_v1: {e}"))?;
    if let Ok(mut got) = outcome.lock() {
        got.bound_dmabuf = true;
    }

    let stride = (W * 4) as u32;
    let file = scratch_file((stride as usize) * (H as usize), &socket)?;

    let params = dmabuf.create_params(&qh, ());
    params.add(
        std::os::fd::AsFd::as_fd(&file),
        0,
        0,
        stride,
        (DRM_FORMAT_MOD_LINEAR >> 32) as u32,
        (DRM_FORMAT_MOD_LINEAR & 0xffff_ffff) as u32,
    );
    params.create(
        W,
        H,
        DRM_FORMAT_XRGB8888,
        zwp_linux_buffer_params_v1::Flags::empty(),
    );

    let mut state = Client { outcome };
    while !stop.load(Ordering::SeqCst) {
        if queue.roundtrip(&mut state).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    Ok(())
}

fn scratch_file(size: usize, near: &std::path::Path) -> Result<std::fs::File, String> {
    let dir = near.parent().unwrap_or(std::path::Path::new("/tmp"));
    let path = dir.join(format!("schorl-dmabuf-probe-{}", std::process::id()));
    let mut file = std::fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("open {path:?}: {e}"))?;
    file.write_all(&vec![0u8; size])
        .map_err(|e| format!("fill {path:?}: {e}"))?;
    let _ = std::fs::remove_file(&path);
    Ok(file)
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
fn a_client_can_hand_schorl_a_dmabuf_and_schorl_passes_it_on() {
    let _runtime_dir = require_runtime_dir();

    let recorder = RecordingImporter::new();
    let ids = Arc::new(Uuidv7IdGen);
    let journal = Journal::with_new_trace(
        Arc::new(SystemClock),
        Arc::new(StderrJsonLogSink),
        ids.as_ref(),
    )
    .expect("a trace id can be issued");
    let mut setup = CompositorSetup::new(ids, journal);
    setup.importer = Box::new(recorder.clone());

    let mut session = HeadlessSession::start(setup).expect("schorl starts");
    let path = session.socket_path().expect("the socket has a path");
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let stop = Arc::new(AtomicBool::new(false));

    let client_outcome = Arc::clone(&outcome);
    let client_stop = Arc::clone(&stop);
    let client = std::thread::spawn(move || run_client(path, client_outcome, client_stop));

    let seen = session
        .pump_until(Duration::from_secs(10), Duration::from_millis(2), |_| {
            let got = outcome.lock().expect("not poisoned");
            got.created || got.failed
        })
        .expect("the compositor runs");

    stop.store(true, Ordering::SeqCst);
    let got = outcome.lock().expect("not poisoned").clone();
    let handed = recorder.dmabufs();
    let kinds: Vec<ClientBufferKind> = recorder.seen().into_iter().map(|(_, k)| k).collect();
    drop(session);
    let joined = client.join();

    eprintln!("client outcome={got:?} handed={handed:?} joined={joined:?}");
    assert!(
        seen,
        "the compositor should have answered the client: {got:?}"
    );
    assert!(got.bound_dmabuf, "zwp_linux_dmabuf_v1 must be advertised");
    assert!(!got.failed, "schorl refused the dmabuf: {got:?}");
    assert!(got.created, "schorl must answer `created`: {got:?}");

    assert_eq!(
        handed.len(),
        1,
        "exactly one dmabuf should have been handed on"
    );
    let described = &handed[0];
    assert_eq!(described.fourcc, DRM_FORMAT_XRGB8888);
    assert_eq!(described.modifier, DRM_FORMAT_MOD_LINEAR);
    assert_eq!(described.width_px, W);
    assert_eq!(described.height_px, H);
    assert_eq!(described.plane_count, 1);
    assert!(kinds.contains(&ClientBufferKind::Dmabuf));
}
