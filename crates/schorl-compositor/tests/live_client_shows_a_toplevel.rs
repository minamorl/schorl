//! 実在のクライアントを一本繋いで、`xdg_toplevel` が出るところまでを実測する。
//!
//! この試験は schorl のソケットに外のプロセスが繋がることを確かめる。
//! `pin wm.own_socket` と `pin v1.window_count = one_or_more` が実際に
//! 効いているかは、ここを通ることでしか分からない。
//!
//! **これは受け入れではない** (`pin verify.no_green_substitute`)。
//! HMD を被った確認の代わりにはならない。
//!
//! クライアントが居ない機械では何も言えないので、その場合は黙って通す。

use std::sync::Arc;
use std::time::Duration;

use schorl_compositor::driver::{HeadlessOptions, run_headless};
use schorl_compositor::journal::{Journal, StderrJsonLogSink, Uuidv7IdGen};
use schorl_compositor::state::CompositorSetup;
use schorl_core::time::SystemClock;

/// この機で試せる軽い xdg-shell クライアントを一つ選ぶ。
fn pick_client() -> Option<Vec<String>> {
    let candidates: [&[&str]; 3] = [
        &["weston-simple-shm"],
        &["foot", "--"],
        &["weston-terminal"],
    ];
    for argv in candidates {
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
    if std::env::var("XDG_RUNTIME_DIR").is_err() {
        eprintln!("no XDG_RUNTIME_DIR: this machine cannot say anything here");
        return;
    }
    let Some(client) = pick_client() else {
        eprintln!("no light xdg-shell client on this machine: nothing measured");
        return;
    };

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
    let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") else {
        return;
    };
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
    let Ok(host) = std::env::var("WAYLAND_DISPLAY") else {
        return;
    };
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
    assert_ne!(
        outcome.socket, host,
        "pin wm.host_compositor_coexistence: the host keeps its own socket"
    );
}

#[test]
fn two_real_clients_get_two_windows_side_by_side() {
    if std::env::var("XDG_RUNTIME_DIR").is_err() {
        return;
    }
    let Some(argv) = pick_client() else {
        eprintln!("no light xdg-shell client on this machine: nothing measured");
        return;
    };

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
