//! `zwlr_screencopy_manager_v1` version 3 で出力を一枚取る実装。
//!
//! 満たす pin:
//! - `v1.panel_source: require schorl.v1.panel.content_source = linux_display` —
//!   [`OutputId`] の綴りを `wl_output` version 4 の `name` イベントと突き合わせて
//!   その出力だけを取る。
//! - `verify.machine_scope` の `capture_returns_real_frame` — ここから返る
//!   [`Frame`] は [`FrameOrigin::RealCapture`] を名乗る。名乗れるのは compositor が
//!   `ready` を送った経路だけで、`failed` も時間切れも封筒になる。
//! - `code.time.tz: require time.storage = utc` — `ready` が運ぶ時刻は
//!   「開始点に任意のずれがあってよい」と protocol が明言している単調時計なので
//!   UTC ではない。[`Frame::captured_at`] には [`Clock`] が返す UTC を入れる。
//! - `code.retry.backoff` / `code.retry.max` — 再試行は [`capture_with_retry`] だけが
//!   行い、待ちは `schorl_core::retry` の指数 + full jitter である。
//! - `house.effect_boundary.form` — 乱数と待ちは引数で受ける。握り込まない。
//!
//! `free schorl.capture.protocol` / `free schorl.capture.protocol_version` の中での
//! 選択なので、spec は wlr-screencopy を要求していない。
//!
//! 一次資料として読んだもの:
//! - `wlr-screencopy-unstable-v1.xml`
//!   (wayland-protocols-wlr 0.3.12 同梱、`interface name="zwlr_screencopy_manager_v1" version="3"`)。
//!   逐語の手順は「frame を作ると `buffer` イベントが並び、最後に `buffer_done` が来る。
//!   client は対応する buffer を作って `copy` を送る。成功すれば `flags` のあと `ready`、
//!   失敗すれば `failed`」。
//! - 同 XML の逐語 `Note! This protocol is deprecated and not intended for production use.
//!   The ext-image-copy-capture-v1 protocol should be used instead.` —
//!   `quarantine capture.deprecation_divergence` と `open_question schorl.capture_successor`
//!   が扱っている乖離であり、実測ホストが advertise しているのはこちらだけである。
//! - `wayland-client` 0.31.15 の `globals` module と `Dispatch` trait。
//! - `wayland.xml` (wayland-client 0.31.15 同梱) の `wl_shm` / `wl_shm_pool` /
//!   `wl_output` version 4 (`name` イベントは `since="4"`)。
//! - `rustix` 1.1.5 の `fs::memfd_create` / `fs::ftruncate` / `event::poll`。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::TraceId;
use schorl_core::retry::RetryPolicy;
use schorl_core::time::Clock;
use schorl_display::OutputId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::{Frame, FrameOrigin, FrameSource, PixelFormat};

/// この実装が要求する `zwlr_screencopy_manager_v1` の版。
///
/// 実測ホストが advertise しているのが 3 で、`buffer_done` が `since="3"` である。
pub const SCREENCOPY_VERSION: u32 = 3;

/// `wl_output` に要求する版。`name` イベントが `since="4"`。
pub const OUTPUT_VERSION: u32 = 4;

/// 一枚取るのに待つ既定の上限。
pub const DEFAULT_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

fn host_refused(message: &'static str) -> Error {
    Error::new(ErrorCode::HostRefused, message, TraceId::unattributed())
}

fn internal(message: &'static str) -> Error {
    Error::new(ErrorCode::Internal, message, TraceId::unattributed())
}

/// compositor が `buffer` イベントで指定してきた wl_shm buffer の形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShmBufferSpec {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

#[derive(Debug, Default)]
struct PendingFrame {
    shm: Option<ShmBufferSpec>,
    unsupported_format: Option<u32>,
    buffer_done: bool,
    ready: bool,
    failed: bool,
    y_invert: bool,
}

#[derive(Debug)]
struct OutputEntry {
    global_name: u32,
    output: wl_output::WlOutput,
    name: Option<String>,
}

/// 事象を溜める箱。`Dispatch` の実装はここにだけ書き込む。
#[derive(Debug, Default)]
struct CaptureState {
    outputs: Vec<OutputEntry>,
    pending: PendingFrame,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CaptureState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_output::WlOutput::interface().name => {
                // 板用の headless 出力は接続より後に生えるので、動的に束ねる。
                let bound = registry.bind::<wl_output::WlOutput, u32, Self>(
                    name,
                    version.min(OUTPUT_VERSION),
                    qh,
                    name,
                );
                state.outputs.push(OutputEntry {
                    global_name: name,
                    output: bound,
                    name: None,
                });
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|entry| entry.global_name != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for CaptureState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        global_name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state
                .outputs
                .iter_mut()
                .find(|entry| entry.global_name == *global_name)
        {
            entry.name = Some(name);
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => match format {
                WEnum::Value(format) if supported_format(format).is_some() => {
                    state.pending.shm = Some(ShmBufferSpec {
                        format,
                        width,
                        height,
                        stride,
                    });
                }
                WEnum::Value(format) => {
                    state.pending.unsupported_format = Some(u32::from(format));
                }
                WEnum::Unknown(raw) => {
                    state.pending.unsupported_format = Some(raw);
                }
            },
            zwlr_screencopy_frame_v1::Event::BufferDone => state.pending.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                if let WEnum::Value(flags) = flags {
                    state.pending.y_invert =
                        flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.pending.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.pending.failed = true,
            // `linux_dmabuf` と `damage` はこの経路では使わない。
            _ => {}
        }
    }
}

delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);
delegate_noop!(CaptureState: ignore ZwlrScreencopyManagerV1);

/// `wl_shm` の format のうち、[`PixelFormat`] に写せるもの。
fn supported_format(format: wl_shm::Format) -> Option<PixelFormat> {
    match format {
        wl_shm::Format::Xrgb8888 => Some(PixelFormat::Xrgb8888),
        wl_shm::Format::Argb8888 => Some(PixelFormat::Argb8888),
        _ => None,
    }
}

/// 実ホストの compositor から一枚取る [`FrameSource`]。
pub struct ScreencopyFrameSource {
    conn: Connection,
    queue: EventQueue<CaptureState>,
    state: CaptureState,
    shm: wl_shm::WlShm,
    manager: ZwlrScreencopyManagerV1,
    clock: Arc<dyn Clock>,
    timeout: Duration,
    overlay_cursor: bool,
    poison_fill: Option<u8>,
}

impl std::fmt::Debug for ScreencopyFrameSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Clock` は `Debug` を要求していない capability なので、名前だけ出す。
        f.debug_struct("ScreencopyFrameSource")
            .field("outputs", &self.state.outputs.len())
            .field("timeout", &self.timeout)
            .field("overlay_cursor", &self.overlay_cursor)
            .finish_non_exhaustive()
    }
}

impl ScreencopyFrameSource {
    /// 環境が指す compositor へ繋いで、必要な global を束ねる。
    ///
    /// `WAYLAND_DISPLAY` と `XDG_RUNTIME_DIR` は `wayland-client` が読む。
    pub fn connect(clock: Arc<dyn Clock>) -> Result<Self> {
        let conn = Connection::connect_to_env().map_err(|source| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "could not connect to a wayland compositor",
                TraceId::unattributed(),
            )
            .caused_by(source)
        })?;
        let (globals, queue) = registry_queue_init::<CaptureState>(&conn).map_err(|source| {
            host_refused("the compositor did not answer the registry roundtrip").caused_by(source)
        })?;
        let qh = queue.handle();

        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|source| {
            Error::new(
                ErrorCode::CapabilityUnavailable,
                "the compositor does not offer wl_shm",
                TraceId::unattributed(),
            )
            .caused_by(source)
        })?;
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, SCREENCOPY_VERSION..=SCREENCOPY_VERSION, ())
            .map_err(|source| {
                Error::new(
                    ErrorCode::CapabilityUnavailable,
                    "the compositor does not offer zwlr_screencopy_manager_v1 version 3",
                    TraceId::unattributed(),
                )
                .caused_by(source)
            })?;

        let mut state = CaptureState::default();
        // 接続時点で既に在る出力を束ねる。あとから生えるものは registry の
        // `global` イベントが拾う。
        let registry = globals.registry().clone();
        for global in globals.contents().clone_list() {
            if global.interface == wl_output::WlOutput::interface().name {
                let bound = registry.bind::<wl_output::WlOutput, u32, CaptureState>(
                    global.name,
                    global.version.min(OUTPUT_VERSION),
                    &qh,
                    global.name,
                );
                state.outputs.push(OutputEntry {
                    global_name: global.name,
                    output: bound,
                    name: None,
                });
            }
        }

        Ok(Self {
            conn,
            queue,
            state,
            shm,
            manager,
            clock,
            timeout: DEFAULT_CAPTURE_TIMEOUT,
            overlay_cursor: false,
            poison_fill: None,
        })
    }

    /// 一枚取るのに待つ上限を変える。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// カーソルを合成させるか。既定は合成しない。
    pub fn with_overlay_cursor(mut self, overlay_cursor: bool) -> Self {
        self.overlay_cursor = overlay_cursor;
        self
    }

    /// 共有メモリを渡す前に一色で埋めておく。既定は埋めない。
    ///
    /// 検出器の較正のために在る。compositor が一画素も書かなかった場合、
    /// 返る絵はこの色のままになるので、**書かれなかったことを見分けられる**。
    /// 埋めない既定では、書かれなかった buffer は零のまま返り、
    /// 「真っ黒な実フレーム」と区別が付かない。
    /// 毎フレーム分の書き込みが増えるので、確認経路だけで使うこと。
    pub fn with_poison_fill(mut self, poison: Option<u8>) -> Self {
        self.poison_fill = poison;
        self
    }

    /// いま設定されている毒の色。
    pub const fn poison_fill(&self) -> Option<u8> {
        self.poison_fill
    }

    /// いま見えている出力の名前。
    pub fn output_names(&mut self) -> Result<Vec<String>> {
        self.sync()?;
        Ok(self
            .state
            .outputs
            .iter()
            .filter_map(|entry| entry.name.clone())
            .collect())
    }

    /// registry と出力名を今の状態へ追いつかせる。
    fn sync(&mut self) -> Result<()> {
        // 一往復目で新しい global を束ね、二往復目でその `name` を受け取る。
        for _ in 0..2 {
            self.queue.roundtrip(&mut self.state).map_err(|source| {
                host_refused("the compositor did not answer a roundtrip").caused_by(source)
            })?;
        }
        Ok(())
    }

    fn find_output(&self, id: &OutputId) -> Result<wl_output::WlOutput> {
        self.state
            .outputs
            .iter()
            .find(|entry| entry.name.as_deref() == Some(id.as_str()))
            .map(|entry| entry.output.clone())
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidArgument,
                    "the compositor does not advertise an output with that name",
                    TraceId::unattributed(),
                )
                .with_detail("output", id.as_str())
            })
    }

    /// `done` が真になるまで事象を回す。締切を越えたら封筒で返る。
    fn pump_until(&mut self, deadline: Instant, done: fn(&PendingFrame) -> bool) -> Result<()> {
        loop {
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(|source| {
                    host_refused("dispatching compositor events failed").caused_by(source)
                })?;
            if done(&self.state.pending) {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(host_refused("the compositor did not answer in time"));
            }
            self.queue.flush().map_err(|source| {
                host_refused("flushing requests to the compositor failed").caused_by(source)
            })?;
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let remaining = deadline.saturating_duration_since(now);
            let timeout = Timespec {
                tv_sec: remaining.as_secs() as _,
                tv_nsec: remaining.subsec_nanos() as _,
            };
            let mut fds = [PollFd::from_borrowed_fd(self.conn.as_fd(), PollFlags::IN)];
            let ready = poll(&mut fds, Some(&timeout)).map_err(|source| {
                internal("polling the wayland socket failed").caused_by(source)
            })?;
            if ready == 0 {
                drop(guard);
                return Err(host_refused("the compositor did not answer in time"));
            }
            guard.read().map_err(|source| {
                host_refused("reading from the wayland socket failed").caused_by(source)
            })?;
        }
    }

    fn capture_inner(&mut self, output: &OutputId) -> Result<Frame> {
        self.sync()?;
        let wl_output = self.find_output(output)?;
        let deadline = Instant::now() + self.timeout;

        self.state.pending = PendingFrame::default();
        let qh = self.queue.handle();
        let frame =
            self.manager
                .capture_output(i32::from(self.overlay_cursor), &wl_output, &qh, ());
        // ここから先のどの経路でも frame を壊して返す
        // (`house.resource_lifecycle.release_paths`)。
        let outcome = self.copy_into_frame(&frame, deadline);
        frame.destroy();
        outcome
    }

    fn copy_into_frame(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        deadline: Instant,
    ) -> Result<Frame> {
        self.pump_until(deadline, |pending| {
            pending.buffer_done || pending.failed || pending.unsupported_format.is_some()
        })?;
        if self.state.pending.failed {
            return Err(host_refused(
                "the compositor refused to capture this output",
            ));
        }
        let spec = match self.state.pending.shm {
            Some(spec) => spec,
            None => {
                let mut err =
                    host_refused("the compositor offered no wl_shm buffer for this frame");
                if let Some(raw) = self.state.pending.unsupported_format {
                    err = err.with_detail("offered_format", i64::from(raw));
                }
                return Err(err);
            }
        };
        if spec.width == 0 || spec.height == 0 {
            return Err(host_refused("the compositor offered a zero-sized buffer"));
        }
        let pixel_format = supported_format(spec.format)
            .ok_or_else(|| host_refused("the compositor offered an unsupported pixel format"))?;
        let size = (spec.stride as usize)
            .checked_mul(spec.height as usize)
            .ok_or_else(|| host_refused("the offered buffer size overflows"))?;

        let memfd = new_shared_memory(size, self.poison_fill)?;
        let qh = self.queue.handle();
        let pool = self.shm.create_pool(memfd.as_fd(), size as i32, &qh, ());
        let buffer = pool.create_buffer(
            0,
            spec.width as i32,
            spec.height as i32,
            spec.stride as i32,
            spec.format,
            &qh,
            (),
        );

        frame.copy(&buffer);
        let waited = self.pump_until(deadline, |pending| pending.ready || pending.failed);

        // 成功・失敗のどちらでも wayland 側の資源を返す。
        buffer.destroy();
        pool.destroy();
        waited?;

        if self.state.pending.failed {
            return Err(host_refused("the compositor reported the copy as failed"));
        }
        if !self.state.pending.ready {
            return Err(host_refused(
                "the compositor never reported the frame as ready",
            ));
        }

        let mut pixels = read_all(memfd, size)?;
        if self.state.pending.y_invert {
            flip_rows(&mut pixels, spec.stride as usize, spec.height as usize);
        }

        Frame::new(
            FrameOrigin::RealCapture,
            pixel_format,
            spec.width,
            spec.height,
            spec.stride,
            self.clock.now_utc(),
            pixels,
        )
    }
}

impl FrameSource for ScreencopyFrameSource {
    fn capture(&mut self, output: &OutputId) -> Result<Frame> {
        self.capture_inner(output)
    }
}

/// compositor と共有する匿名メモリを一つ作る。
///
/// `poison` を渡すと、compositor へ見せる前に全体をその値で埋める。
fn new_shared_memory(size: usize, poison: Option<u8>) -> Result<OwnedFd> {
    let fd = memfd_create("schorl-capture", MemfdFlags::CLOEXEC).map_err(|source| {
        internal("could not create the shared memory for the frame").caused_by(source)
    })?;
    ftruncate(&fd, size as u64).map_err(|source| {
        internal("could not size the shared memory for the frame").caused_by(source)
    })?;
    if let Some(poison) = poison {
        let mut file = File::from(fd);
        file.write_all(&vec![poison; size]).map_err(|source| {
            internal("could not poison the shared memory before the copy").caused_by(source)
        })?;
        file.flush().map_err(|source| {
            internal("could not flush the poisoned shared memory").caused_by(source)
        })?;
        return Ok(OwnedFd::from(file));
    }
    Ok(fd)
}

/// 共有メモリの中身を読み出す。
///
/// `mmap` を使わないのは、`unsafe` を持ち込まずに同じ頁を読めるからである。
fn read_all(fd: OwnedFd, size: usize) -> Result<Vec<u8>> {
    let mut file = File::from(fd);
    file.seek(SeekFrom::Start(0))
        .map_err(|source| internal("could not rewind the frame memory").caused_by(source))?;
    let mut pixels = vec![0_u8; size];
    file.read_exact(&mut pixels)
        .map_err(|source| internal("could not read the captured frame back").caused_by(source))?;
    Ok(pixels)
}

/// 行の並びを上下反転する (`y_invert` フラグへの対応)。
fn flip_rows(pixels: &mut [u8], stride: usize, height: usize) {
    if stride == 0 || height < 2 {
        return;
    }
    for row in 0..height / 2 {
        let top = row * stride;
        let bottom = (height - 1 - row) * stride;
        for byte in 0..stride {
            pixels.swap(top + byte, bottom + byte);
        }
    }
}

/// 待ちと乱数を外から受け取る再試行つきの捕捉。
///
/// `code.retry.backoff: require retry.backoff = exponential_jitter` と
/// `code.retry.max: range retry.max = 0..5` は [`RetryPolicy`] が型で持っている。
/// 乱数源と時間を潰す手段はここでは握らず、引数で受ける
/// (`house.effect_boundary.form`)。`jitter` は `[0,1)` の一様値を返すこと。
pub fn capture_with_retry(
    source: &mut dyn FrameSource,
    output: &OutputId,
    policy: RetryPolicy,
    jitter: &mut dyn FnMut() -> f64,
    sleep: &mut dyn FnMut(Duration),
) -> Result<Frame> {
    let mut attempts_made: u8 = 0;
    loop {
        match source.capture(output) {
            Ok(frame) => return Ok(frame),
            Err(err) => {
                if !policy.should_retry(attempts_made) {
                    return Err(err);
                }
                let delay = policy.delay_millis(u32::from(attempts_made), jitter());
                sleep(Duration::from_millis(delay));
                attempts_made = attempts_made.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::SolidColourFrameSource;
    use schorl_core::retry::Backoff;
    use schorl_core::time::UtcTimestamp;
    use std::cell::RefCell;

    #[test]
    fn only_the_two_formats_the_frame_type_knows_are_accepted() {
        assert_eq!(
            supported_format(wl_shm::Format::Xrgb8888),
            Some(PixelFormat::Xrgb8888)
        );
        assert_eq!(
            supported_format(wl_shm::Format::Argb8888),
            Some(PixelFormat::Argb8888)
        );
        assert_eq!(supported_format(wl_shm::Format::Rgb565), None);
    }

    #[test]
    fn flipping_rows_undoes_itself() {
        let original: Vec<u8> = (0..24).collect();
        let mut pixels = original.clone();
        flip_rows(&mut pixels, 4, 6);
        assert_eq!(&pixels[0..4], &original[20..24]);
        assert_eq!(&pixels[20..24], &original[0..4]);
        flip_rows(&mut pixels, 4, 6);
        assert_eq!(pixels, original);
    }

    #[test]
    fn flipping_an_odd_height_keeps_the_middle_row() {
        let original: Vec<u8> = (0..15).collect();
        let mut pixels = original.clone();
        flip_rows(&mut pixels, 3, 5);
        assert_eq!(&pixels[6..9], &original[6..9]);
        assert_eq!(&pixels[0..3], &original[12..15]);
    }

    /// 最初の `failures` 回だけ失敗する贋物。
    #[derive(Debug)]
    struct FlakyFrameSource {
        remaining_failures: u32,
        attempts: u32,
        inner: SolidColourFrameSource,
    }

    impl FrameSource for FlakyFrameSource {
        fn capture(&mut self, output: &OutputId) -> Result<Frame> {
            self.attempts += 1;
            if self.remaining_failures > 0 {
                self.remaining_failures -= 1;
                return Err(host_refused("not yet"));
            }
            self.inner.capture(output)
        }
    }

    fn flaky(failures: u32) -> FlakyFrameSource {
        FlakyFrameSource {
            remaining_failures: failures,
            attempts: 0,
            inner: SolidColourFrameSource {
                width_px: 2,
                height_px: 2,
                fill: 0x11,
                captured_at: UtcTimestamp::from_millis_since_epoch(0),
            },
        }
    }

    fn policy(max: u8) -> RetryPolicy {
        RetryPolicy::new(
            max,
            Backoff::ExponentialJitter {
                base_millis: 10,
                cap_millis: 100,
            },
        )
        .expect("inside the pinned range")
    }

    #[test]
    fn retrying_gives_up_after_the_pinned_number_of_attempts() {
        let mut source = flaky(u32::MAX);
        let slept = RefCell::new(Vec::new());
        let err = capture_with_retry(
            &mut source,
            &OutputId::new("SCHORL-1").expect("valid name"),
            policy(3),
            &mut || 1.0,
            &mut |d| slept.borrow_mut().push(d),
        )
        .expect_err("always fails");
        assert_eq!(err.code(), ErrorCode::HostRefused);
        assert_eq!(source.attempts, 4, "one attempt plus three retries");
        assert_eq!(
            slept.borrow().clone(),
            vec![
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40)
            ],
            "full jitter at its ceiling doubles each time until the cap"
        );
    }

    #[test]
    fn retrying_returns_the_first_frame_that_arrives() {
        let mut source = flaky(2);
        let frame = capture_with_retry(
            &mut source,
            &OutputId::new("SCHORL-1").expect("valid name"),
            policy(5),
            &mut || 0.0,
            &mut |_| {},
        )
        .expect("the third attempt works");
        assert_eq!(source.attempts, 3);
        // 贋物から来た絵は実捕捉を名乗らない。
        assert!(!frame.is_real_capture());
    }

    #[test]
    fn a_policy_that_forbids_retries_asks_exactly_once() {
        let mut source = flaky(1);
        let err = capture_with_retry(
            &mut source,
            &OutputId::new("SCHORL-1").expect("valid name"),
            policy(0),
            &mut || 0.5,
            &mut |_| panic!("must not sleep"),
        )
        .expect_err("no retries allowed");
        assert_eq!(err.code(), ErrorCode::HostRefused);
        assert_eq!(source.attempts, 1);
    }
}
