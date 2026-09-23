//! compositor 本体の状態と、Wayland の各 global の受け口。
//!
//! `pin wm.role: require schorl.role = wayland_compositor` の実体。
//! ここが `wl_compositor` / `xdg_wm_base` / `wl_seat` / `wl_shm` /
//! `zwp_linux_dmabuf_v1` / `wl_output` / `wl_data_device_manager` を出す。

use std::collections::VecDeque;
use std::sync::Arc;

use schorl_core::error::{Error, ErrorCode, Result};
use schorl_core::id::IdGen;

use smithay::backend::allocator::Format as DrmFormat;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::utils::{Serial, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
    with_states,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{BufferAccessError, ShmHandler, ShmState, with_buffer_contents};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat,
    delegate_shm, delegate_xdg_shell,
};

use crate::buffer::{ClientBufferKind, ClientTextureImporter, ShmBufferView};
use crate::journal::Journal;
use crate::window::{RingPlacement, WindowId, WindowRegistry};

/// 一つのクライアント接続に紐づくデータ。
#[derive(Debug, Default)]
pub struct CompositorClientData {
    /// smithay が surface の状態を吊るす先。
    pub compositor_state: CompositorClientState,
}

impl smithay::reexports::wayland_server::backend::ClientData for CompositorClientData {}

/// dmabuf global に出す既定の format。
///
/// `free schorl.compositor.buffer_import_path` なので選択である。実際に取り込む面が
/// 決まったら、その renderer が言う format 一覧へ差し替えること。ここは
/// 「受け口を開ける」ための最小の申告にすぎない。
pub fn default_dmabuf_formats() -> Vec<DrmFormat> {
    use smithay::backend::allocator::{Fourcc, Modifier};
    let codes = [Fourcc::Argb8888, Fourcc::Xrgb8888];
    let modifiers = [Modifier::Invalid, Modifier::Linear];
    let mut formats = Vec::with_capacity(codes.len() * modifiers.len());
    for code in codes {
        for modifier in modifiers {
            formats.push(DrmFormat { code, modifier });
        }
    }
    formats
}

/// 同じ鍵の再送を落とすための覚え。
///
/// `pin code.idempotency.write: require write.idempotency = required`。
/// 無制限に溜めると漏れるので、直近 [`Self::CAPACITY`] 件だけを持つ輪にしてある。
#[derive(Debug, Clone, Default)]
pub struct DeliveredKeys {
    order: VecDeque<String>,
}

impl DeliveredKeys {
    /// 覚えておく件数。
    pub const CAPACITY: usize = 1024;

    /// 空で作る。
    pub const fn new() -> Self {
        Self {
            order: VecDeque::new(),
        }
    }

    /// 初めて見た鍵なら覚えて `true`。既に見ていたら `false`。
    pub fn admit(&mut self, key: &str) -> bool {
        if self.order.iter().any(|seen| seen == key) {
            return false;
        }
        if self.order.len() == Self::CAPACITY {
            self.order.pop_front();
        }
        self.order.push_back(key.to_owned());
        true
    }
}

/// schorl の compositor 状態。
pub struct SchorlCompositor {
    pub(crate) display: DisplayHandle,
    pub(crate) compositor_state: CompositorState,
    pub(crate) xdg_shell_state: XdgShellState,
    pub(crate) shm_state: ShmState,
    pub(crate) dmabuf_state: DmabufState,
    pub(crate) dmabuf_global: DmabufGlobal,
    pub(crate) data_device_state: DataDeviceState,
    pub(crate) seat_state: SeatState<Self>,
    pub(crate) seat: Seat<Self>,
    pub(crate) output: Output,
    pub(crate) windows: WindowRegistry<ToplevelSurface>,
    pub(crate) importer: Box<dyn ClientTextureImporter>,
    pub(crate) ids: Arc<dyn IdGen>,
    pub(crate) journal: Journal,
    pub(crate) default_window_size: (i32, i32),
    pub(crate) pointer_target: Option<WindowId>,
    pub(crate) delivered: DeliveredKeys,
    pub(crate) running: bool,
}

impl std::fmt::Debug for SchorlCompositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchorlCompositor")
            .field("windows", &self.windows.len())
            .field("seat", &self.seat.name())
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

/// [`SchorlCompositor`] を組み立てるための材料。
///
/// 周囲効果 (時刻・ログ・識別子・画素の行き先) はすべてここで注入する
/// (`house.effect_boundary.location = injected_at_boundary`)。
pub struct CompositorSetup {
    /// 識別子の発行。
    pub ids: Arc<dyn IdGen>,
    /// ログの束。
    pub journal: Journal,
    /// クライアントの画素の引き渡し先。
    pub importer: Box<dyn ClientTextureImporter>,
    /// ウィンドウの置き方。
    pub placement: RingPlacement,
    /// toplevel へ最初に送る大きさ (画素)。`free schorl.window.size`。
    pub default_window_size: (i32, i32),
    /// seat の名前。
    pub seat_name: String,
    /// dmabuf global に申告する format。
    pub dmabuf_formats: Vec<DrmFormat>,
}

impl std::fmt::Debug for CompositorSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositorSetup")
            .field("placement", &self.placement)
            .field("default_window_size", &self.default_window_size)
            .field("seat_name", &self.seat_name)
            .field("dmabuf_formats", &self.dmabuf_formats.len())
            .finish_non_exhaustive()
    }
}

impl CompositorSetup {
    /// 既定の材料。画素の行き先は数えるだけの記録係になる。
    pub fn new(ids: Arc<dyn IdGen>, journal: Journal) -> Self {
        Self {
            ids,
            journal,
            importer: Box::new(crate::buffer::RecordingImporter::new()),
            placement: RingPlacement::DEFAULT,
            default_window_size: (1280, 720),
            seat_name: "schorl".to_owned(),
            dmabuf_formats: default_dmabuf_formats(),
        }
    }
}

impl SchorlCompositor {
    /// 全ての global を出して状態を組む。
    pub fn new(display: &DisplayHandle, setup: CompositorSetup) -> Result<Self> {
        let CompositorSetup {
            ids,
            journal,
            importer,
            placement,
            default_window_size,
            seat_name,
            dmabuf_formats,
        } = setup;

        let compositor_state = CompositorState::new::<Self>(display);
        let xdg_shell_state = XdgShellState::new::<Self>(display);
        let shm_state = ShmState::new::<Self>(display, Vec::new());
        let data_device_state = DataDeviceState::new::<Self>(display);
        let mut dmabuf_state = DmabufState::new();
        let dmabuf_global = dmabuf_state.create_global::<Self>(display, dmabuf_formats);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(display, seat_name);
        let _pointer = seat.add_pointer();
        // 配列は既定。`free` の軸であり、v1 では宿主から鍵を借りない
        // (schorl 自身の backend から取る)。
        seat.add_keyboard(Default::default(), 200, 25)
            .map_err(|e| {
                Error::new(
                    ErrorCode::CapabilityUnavailable,
                    "could not create the keyboard for schorl's own seat",
                    journal.trace_id().clone(),
                )
                .caused_by(e)
            })?;

        // ウィンドウは 360 度の空間に立つので、物理モニタに対応する出力は無い。
        // それでも wl_output を一つ出すのは、toolkit の多くが出力の存在を前提に
        // surface を map するためである (`free schorl.client.connection_mechanism`)。
        let output = Output::new(
            "schorl-virtual".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "schorl".to_owned(),
                model: "virtual".to_owned(),
            },
        );
        let _output_global = output.create_global::<Self>(display);
        let mode = Mode {
            size: (default_window_size.0, default_window_size.1).into(),
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);

        Ok(Self {
            display: display.clone(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            dmabuf_state,
            dmabuf_global,
            data_device_state,
            seat_state,
            seat,
            output,
            windows: WindowRegistry::new(placement),
            importer,
            ids,
            journal,
            default_window_size,
            pointer_target: None,
            delivered: DeliveredKeys::new(),
            running: true,
        })
    }

    /// ログの束。
    pub const fn journal(&self) -> &Journal {
        &self.journal
    }

    /// 追跡しているウィンドウ。
    pub const fn windows(&self) -> &WindowRegistry<ToplevelSurface> {
        &self.windows
    }

    /// 追跡しているウィンドウ (書き換え用)。掴んで置き直す面が使う。
    pub const fn windows_mut(&mut self) -> &mut WindowRegistry<ToplevelSurface> {
        &mut self.windows
    }

    /// display への取っ手。新しいクライアントを入れる面が使う。
    pub const fn display(&self) -> &DisplayHandle {
        &self.display
    }

    /// dmabuf global。取り込む面が feedback を差し替えるときに要る。
    pub const fn dmabuf_global(&self) -> &DmabufGlobal {
        &self.dmabuf_global
    }

    /// schorl が持つ seat。
    pub const fn seat(&self) -> &Seat<Self> {
        &self.seat
    }

    /// クライアントへ見せている仮想出力。
    pub const fn output(&self) -> &Output {
        &self.output
    }

    /// まだ回すか。
    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// 次の回で止める。
    pub const fn stop(&mut self) {
        self.running = false;
    }

    /// 新しい serial。
    pub fn next_serial(&self) -> Serial {
        smithay::utils::SERIAL_COUNTER.next_serial()
    }

    /// ログ行が出せなかったこと自体では compositor を止めない。
    ///
    /// 行は既に組み立てられていて、落ちたのは行き先への書き出しだけである。
    /// そこで compositor を落とすとクライアントが巻き添えになるので、ここで畳む。
    /// 封筒は作られているので `code.error.envelope` の形は崩れていない。
    fn ignore_log_failure(&self, emitted: Result<()>) {
        let _ = emitted;
    }

    /// commit の中でバッファを引き渡す。
    fn hand_off_buffer(&mut self, surface: &WlSurface, buffer: &WlBuffer) {
        let Some(window) = self
            .windows
            .find(|toplevel| toplevel.wl_surface() == surface)
            .map(|w| w.id().clone())
        else {
            return;
        };

        if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(buffer) {
            let outcome = self.importer.import_dmabuf(Some(&window), dmabuf);
            self.note_handoff(&window, ClientBufferKind::Dmabuf, outcome);
            return;
        }

        let read = with_buffer_contents(buffer, |ptr, len, data| {
            // SAFETY: smithay が mmap した pool の範囲であり、この閉包が返るまで有効。
            // 借用はこの閉包の外へ出さない (`house.resource_lifecycle.same_scope`)。
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
            self.importer.import_shm(
                &window,
                ShmBufferView {
                    wl_shm_format: data.format as u32,
                    width_px: data.width,
                    height_px: data.height,
                    stride_bytes: data.stride,
                    offset_bytes: data.offset,
                    bytes,
                },
            )
        });

        match read {
            Ok(outcome) => self.note_handoff(&window, ClientBufferKind::Shm, outcome),
            Err(BufferAccessError::NotManaged) => {
                // shm でも dmabuf でもない型 (single-pixel buffer など)。
                // 受け口が無いことは封筒で言い、クライアントは落とさない。
                self.ignore_log_failure(self.journal.warn(format!(
                    "window {} committed a buffer of a kind schorl does not accept yet",
                    window.as_str()
                )));
            }
            Err(e) => {
                let envelope = Error::new(
                    ErrorCode::HostRefused,
                    "the client's shm pool could not be read",
                    self.journal.trace_id().clone(),
                )
                .with_detail("reason", format!("{e}"));
                self.note_handoff(&window, ClientBufferKind::Shm, Err(envelope));
            }
        }
    }

    fn note_handoff(&self, window: &WindowId, kind: ClientBufferKind, outcome: Result<()>) {
        let line = match &outcome {
            Ok(()) => self.journal.info(format!(
                "handed a {} buffer of window {} to the importer",
                kind.as_str(),
                window.as_str()
            )),
            Err(e) => self.journal.note_failure(
                &format!(
                    "handing a {} buffer of window {} to the importer",
                    kind.as_str(),
                    window.as_str()
                ),
                e,
            ),
        };
        self.ignore_log_failure(line);
    }
}

impl CompositorHandler for SchorlCompositor {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        client
            .get_data::<CompositorClientData>()
            .map(|data| &data.compositor_state)
            .expect("every client schorl accepts is inserted with CompositorClientData")
    }

    fn commit(&mut self, surface: &WlSurface) {
        let attached = with_states(surface, |states| {
            match states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .buffer
            {
                Some(BufferAssignment::NewBuffer(ref buffer)) => Some(buffer.clone()),
                _ => None,
            }
        });

        // smithay 側の surface 状態を先に進める。ここでバッファは
        // RendererSurfaceState へ移るので、控えは上で取っておく。
        on_commit_buffer_handler::<Self>(surface);

        if let Some(buffer) = attached {
            if let Some(window) = self
                .windows
                .find_mut(|toplevel| toplevel.wl_surface() == surface)
            {
                let first = !window.is_mapped();
                window.mark_mapped();
                if first {
                    let id = window.id().clone();
                    self.ignore_log_failure(
                        self.journal
                            .info(format!("window {} is now mapped", id.as_str())),
                    );
                    let serial = self.next_serial();
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        keyboard.set_focus(self, Some(surface.clone()), serial);
                    }
                }
            }
            self.hand_off_buffer(surface, &buffer);
        }
    }
}

impl BufferHandler for SchorlCompositor {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for SchorlCompositor {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for SchorlCompositor {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // **ここでは GPU へ取り込まない。** 取り込む面は別にあり、この crate は
        // 受けて引き渡す口までを持つ。この時点の dmabuf はまだどの surface へも
        // attach されていないので、宛先ウィンドウは `None` で渡す。
        let planes = dmabuf.num_planes();
        match self.importer.import_dmabuf(None, &dmabuf) {
            Ok(()) => {
                self.ignore_log_failure(
                    self.journal
                        .info(format!("accepted a dmabuf ({planes} planes) for handoff")),
                );
                let _ = notifier.successful::<Self>();
            }
            Err(e) => {
                self.ignore_log_failure(self.journal.note_failure("accepting a dmabuf", &e));
                notifier.failed();
            }
        }
    }
}

impl XdgShellHandler for SchorlCompositor {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

        let size = self.default_window_size;
        surface.with_pending_state(|state| {
            state.size = Some((size.0, size.1).into());
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();

        let id = match self.ids.next_id() {
            Ok(id) => WindowId::new(id),
            Err(e) => {
                self.ignore_log_failure(self.journal.note_failure("issuing a window id", &e));
                return;
            }
        };

        match self.windows.insert(id.clone(), surface, size) {
            Ok(window) => {
                let rect = window.rect();
                let pose = window.plane().pose().pose.position;
                self.ignore_log_failure(self.journal.info(format!(
                    "tracking toplevel {} at atlas ({},{}) and world ({:.2},{:.2},{:.2})",
                    id.as_str(),
                    rect.x,
                    rect.y,
                    pose.x,
                    pose.y,
                    pose.z
                )));
            }
            Err(e) => {
                self.ignore_log_failure(self.journal.note_failure("placing a new toplevel", &e))
            }
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let gone = self
            .windows
            .remove(|tracked| tracked.wl_surface() == surface.wl_surface());
        if let Some(window) = gone {
            if self.pointer_target.as_ref() == Some(window.id()) {
                self.pointer_target = None;
            }
            self.ignore_log_failure(
                self.journal
                    .info(format!("window {} is gone", window.id().as_str())),
            );
        }
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        // popup とサブサーフェスの扱いは v1 の未定事項なので、受けるが置かない。
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

impl smithay::input::SeatHandler for SchorlCompositor {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let named = focused
            .and_then(|surface| {
                self.windows
                    .find(|toplevel| toplevel.wl_surface() == surface)
                    .map(|w| w.id().as_str().to_owned())
            })
            .unwrap_or_else(|| "none".to_owned());
        self.ignore_log_failure(
            self.journal
                .info(format!("keyboard focus is now on {named}")),
        );
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl SelectionHandler for SchorlCompositor {
    type SelectionUserData = ();
}

impl DataDeviceHandler for SchorlCompositor {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for SchorlCompositor {}

impl ServerDndGrabHandler for SchorlCompositor {
    fn send(&mut self, _mime_type: String, _fd: std::os::unix::io::OwnedFd, _seat: Seat<Self>) {}
}

impl OutputHandler for SchorlCompositor {}

delegate_compositor!(SchorlCompositor);
delegate_xdg_shell!(SchorlCompositor);
delegate_shm!(SchorlCompositor);
delegate_dmabuf!(SchorlCompositor);
delegate_seat!(SchorlCompositor);
delegate_data_device!(SchorlCompositor);
delegate_output!(SchorlCompositor);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_idempotency_key_is_only_admitted_once() {
        let mut keys = DeliveredKeys::new();
        assert!(keys.admit("k-1"));
        assert!(!keys.admit("k-1"));
        assert!(keys.admit("k-2"));
    }

    #[test]
    fn the_key_memory_is_bounded() {
        let mut keys = DeliveredKeys::new();
        for n in 0..(DeliveredKeys::CAPACITY + 10) {
            assert!(keys.admit(&format!("k-{n}")));
        }
        // 最初の鍵は忘れているので、もう一度通る。
        assert!(keys.admit("k-0"));
    }

    #[test]
    fn the_default_dmabuf_formats_are_not_empty() {
        assert!(!default_dmabuf_formats().is_empty());
    }
}
