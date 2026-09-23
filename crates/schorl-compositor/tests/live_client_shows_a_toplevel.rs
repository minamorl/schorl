//! 実在のクライアントを一本繋いで、`xdg_toplevel` が出るところまでを実測する。
//!
//! この試験は schorl のソケットに外のプロセスが繋がることを確かめる。
//! `pin wm.own_socket` と `pin v1.window_count = one_or_more` が実際に
//! 効いているかは、ここを通ることでしか分からない。
//!
//! **これは受け入れではない** (`pin verify.no_green_substitute`)。
//! HMD を被った確認の代わりにはならない。
//!
//! **前提が欠けたときに緑を返さない。** クライアントの居ない機械・ソケットを
//! 置く場所の無い機械では、何も測れなかったことを `ok` として数えず、何が無くて
//! 走れないかを名指しして落ちる。以前はここで黙って `return` していた。

use std::sync::Arc;
use std::time::Duration;

use schorl_compositor::driver::{HeadlessOptions, run_headless};
use schorl_compositor::journal::{Journal, StderrJsonLogSink, Uuidv7IdGen};
use schorl_compositor::state::CompositorSetup;
use schorl_core::time::SystemClock;

/// 試せる軽い xdg-shell クライアントの候補。落ちるときに名指しするので定数に出す。
const CLIENT_CANDIDATES: [&[&str]; 3] = [
    &["weston-simple-shm"],
    &["foot", "--"],
    &["weston-terminal"],
];

/// この機で試せる軽い xdg-shell クライアントを一つ選ぶ。
fn pick_client() -> Option<Vec<String>> {
    for argv in CLIENT_CANDIDATES {
        let program = argv[0];
        if which(program) {
            let mut v: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
            if program == "foot" {
                v.push("sleep".to_owned());
                v.push("30".to_owned());
            }
            return Some(v);
        }
    }
    None
}

/// 前提の門。**欠けていたら緑を返さず、何が無いかを名指しして落ちる。**
fn require_runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        panic!(
            "XDG_RUNTIME_DIR is not set, so schorl cannot place its own wayland socket \
             and this check would measure nothing. run it inside a user session, or point \
             XDG_RUNTIME_DIR at a writable directory."
        )
    })
}

/// 同じく前提の門。クライアントが一本も無ければ測れないので、そのときは落ちる。
fn require_client() -> Vec<String> {
    pick_client().unwrap_or_else(|| {
        let names: Vec<&str> = CLIENT_CANDIDATES.iter().map(|argv| argv[0]).collect();
        panic!(
            "no light xdg-shell client on this machine, so nothing would be measured. \
             install one of {names:?} (weston-simple-shm and weston-terminal come from the \
             `weston` package, foot from `foot`)."
        )
    })
}

fn which(program: &str) -> bool {
    std::env::var("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
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

#[test]
fn a_real_client_connects_to_schorls_own_socket_and_maps_a_toplevel() {
    let _runtime_dir = require_runtime_dir();
    let client = require_client();

    let outcome = run_headless(
        setup(),
        HeadlessOptions {
            client: client.clone(),
            run_for: Duration::from_secs(20),
            stop_once_mapped: true,
            ..HeadlessOptions::default()
        },
    )
    .expect("the compositor runs");

    eprintln!("client={client:?} outcome={outcome:?}");
    assert!(
        outcome.clients_accepted > 0,
        "a client should have reached schorl's socket: {outcome:?}"
    );
    assert!(
        outcome.toplevels_seen > 0,
        "the client should have created an xdg_toplevel: {outcome:?}"
    );
    assert!(
        outcome.mapped_windows > 0,
        "the toplevel should have committed a buffer: {outcome:?}"
    );
}

#[test]
fn the_socket_file_is_gone_once_the_run_is_over() {
    let runtime_dir = require_runtime_dir();
    let outcome = run_headless(
        setup(),
        HeadlessOptions {
            client: Vec::new(),
            run_for: Duration::from_millis(120),
            stop_once_mapped: false,
            ..HeadlessOptions::default()
        },
    )
    .expect("the compositor runs");

    let path = std::path::Path::new(&runtime_dir).join(&outcome.socket);
    assert!(
        !path.exists(),
        "{path:?} must be gone: pin host.created_resource_lifecycle"
    );
    let lock = std::path::Path::new(&runtime_dir).join(format!("{}.lock", outcome.socket));
    assert!(!lock.exists(), "{lock:?} must be gone too");
}

#[test]
fn schorl_never_takes_the_host_compositors_socket() {
    let _runtime_dir = require_runtime_dir();

    // 宿主の居ない機械でも測れるように、**前提を自分で用意する**。先に一本立てて
    // 座を一つ埋め、そのうえで測る側を立てる。以前はここで `WAYLAND_DISPLAY` が
    // 無ければ黙って `return` しており、宿主の無い機械では空振りの緑だった。
    let occupant = schorl_compositor::driver::HeadlessSession::start(setup())
        .expect("a first schorl takes a socket");
    let occupied = occupant.socket_name().to_owned();

    let outcome = run_headless(
        setup(),
        HeadlessOptions {
            client: Vec::new(),
            run_for: Duration::from_millis(80),
            stop_once_mapped: false,
            ..HeadlessOptions::default()
        },
    )
    .expect("the compositor runs");

    eprintln!(
        "the second schorl took {:?}; the first one holds {occupied:?}",
        outcome.socket
    );
    assert_ne!(
        outcome.socket, occupied,
        "pin wm.host_compositor_coexistence: a socket someone already holds is not schorl's \
         to take"
    );
    // 宿主が居る機械では、その宿主の座も名指しで測る。
    if let Ok(host) = std::env::var("WAYLAND_DISPLAY") {
        assert_ne!(
            outcome.socket, host,
            "pin wm.host_compositor_coexistence: the host keeps its own socket"
        );
    }
    drop(occupant);
}

#[test]
fn two_real_clients_get_two_windows_side_by_side() {
    let _runtime_dir = require_runtime_dir();
    let argv = require_client();

    let mut session =
        schorl_compositor::driver::HeadlessSession::start(setup()).expect("schorl starts");
    let socket = session.socket_name().to_owned();

    let mut children = Vec::new();
    for _ in 0..2 {
        let child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("WAYLAND_DISPLAY", &socket)
            .env_remove("DISPLAY")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the client starts");
        children.push(child);
    }

    let both = session
        .pump_until(
            std::time::Duration::from_secs(25),
            std::time::Duration::from_millis(2),
            |state| state.windows().iter().filter(|w| w.is_mapped()).count() >= 2,
        )
        .expect("the compositor runs");

    let rects: Vec<_> = session
        .state()
        .windows()
        .iter()
        .map(|w| (w.id().as_str().to_owned(), w.rect()))
        .collect();
    let poses: Vec<_> = session
        .state()
        .windows()
        .iter()
        .map(|w| w.plane().pose().pose.position)
        .collect();

    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
    drop(session);

    eprintln!("rects={rects:?} poses={poses:?}");
    assert!(
        both,
        "two clients should have produced two mapped windows: {rects:?}"
    );
    assert_eq!(
        rects.len(),
        2,
        "pin v1.window_count is one_or_more, not one"
    );
    assert!(
        !rects[0].1.overlaps(&rects[1].1),
        "the two windows must not share atlas pixels: {rects:?}"
    );
    assert!(
        (poses[0].x - poses[1].x).abs() > 0.01,
        "the two windows must stand at different places on the ring: {poses:?}"
    );
}
