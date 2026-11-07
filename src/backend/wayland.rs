use std::{
    error::Error,
    os::fd::{AsFd, AsRawFd},
    sync::Arc,
    time::{Duration, Instant},
};

use memmap2::MmapMut;
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
        wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use xkbcommon::xkb;

use super::{
    Anchors, Backend, InputEvent, Key, KeyboardInteractivity, Layer, MouseButton, OutputSelection,
    PositionF64, Shell, Size, SurfaceConfig, SurfaceId,
};

const WIDTH_FALLBACK: u32 = 1280;
const HEIGHT_FALLBACK: u32 = 1;
const REDRAW_INTERVAL: Duration = Duration::from_millis(16);
const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(600);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(40);

struct RepeatingKey {
    keycode: u32,
    surface: usize,
    next: Instant,
}

/// Wayland layer-shell backend for panels, notifications, launchers and other
/// compositor-managed GUI surfaces.
#[derive(Default)]
pub struct WaylandBackend;

impl Backend for WaylandBackend {
    fn run(&mut self, shell: &mut dyn Shell) -> Result<(), crate::Error> {
        run(shell).map_err(|source| crate::Error::Backend {
            backend: "Wayland",
            source,
        })
    }
}

fn run(shell: &mut dyn Shell) -> Result<(), Box<dyn Error>> {
    let configs = shell.surface_configs().to_vec();

    if configs.is_empty() {
        return Err("shell has no surfaces".into());
    }

    let connection = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<State>(&connection)?;
    let qh = event_queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ())?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ())?;

    let needs_output_binding = configs.iter().any(|config| {
        matches!(
            &config.output,
            OutputSelection::All | OutputSelection::Named(_)
        )
    });
    let registry = globals.registry().clone();

    let outputs = if needs_output_binding {
        globals.contents().with_list(|list| {
            list.iter()
                .filter(|global| global.interface == "wl_output")
                .map(|global| {
                    registry.bind::<wl_output::WlOutput, _, _>(
                        global.name,
                        global.version.min(4),
                        &qh,
                        (),
                    )
                })
                .collect::<Vec<_>>()
        })
    } else {
        Vec::new()
    };

    if needs_output_binding && outputs.is_empty() {
        return Err("Wayland compositor advertised no outputs".into());
    }

    let pointer = seat.get_pointer(&qh, ());
    let keyboard = configs
        .iter()
        .any(|config| config.keyboard_interactivity != KeyboardInteractivity::None)
        .then(|| seat.get_keyboard(&qh, ()));
    let mut state = State::new(shm, outputs.clone(), Vec::new(), seat, pointer, keyboard);
    // wl_output names are delivered asynchronously, so resolve named
    // outputs before constructing their layer surfaces.
    event_queue.roundtrip(&mut state)?;

    add_surfaces(
        &configs,
        &outputs,
        &mut state,
        &compositor,
        &layer_shell,
        &qh,
    )?;
    event_queue.roundtrip(&mut state)?;
    state.draw_pending(&qh, shell)?;

    while !state.closed && !shell.should_close() {
        event_queue.dispatch_pending(&mut state)?;
        connection.flush()?;

        if let Some(guard) = event_queue.prepare_read() {
            let mut fd = libc::pollfd {
                fd: connection.backend().poll_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // A bounded wait lets external controllers close a visible surface.
            let timeout = state
                .repeating_key
                .as_ref()
                .map(|repeat| repeat.next.saturating_duration_since(Instant::now()))
                .map_or(50, |remaining| remaining.as_millis().clamp(1, 50) as i32);

            // SAFETY: fd points to one initialized pollfd for a live connection.
            let ready = unsafe { libc::poll(&raw mut fd, 1, timeout) };

            if ready > 0 {
                guard.read()?;
                event_queue.dispatch_pending(&mut state)?;
            } else if ready < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }

        state.repeat_key();

        if shell.take_redraw_request() {
            state.mark_all_for_redraw();
        }

        let pointer_events = std::mem::take(&mut state.pending_events);

        if !pointer_events.is_empty() {
            state.mark_all_for_redraw();
        }

        for pending in pointer_events {
            shell.handle_event(pending.surface, pending.event);
        }

        if shell.should_close() {
            break;
        }

        state.draw_pending(&qh, shell)?;
    }

    Ok(())
}

fn add_surfaces(
    configs: &[SurfaceConfig],
    outputs: &[wl_output::WlOutput],
    state: &mut State,
    compositor: &wl_compositor::WlCompositor,
    layer_shell: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
    qh: &QueueHandle<State>,
) -> Result<(), Box<dyn Error>> {
    for (index, config) in configs.iter().enumerate() {
        let surface_id = SurfaceId(index);

        match &config.output {
            OutputSelection::All => {
                for output in outputs {
                    let mut surface = create_surface(
                        compositor,
                        layer_shell,
                        Some(output),
                        surface_id,
                        config,
                        qh,
                    );

                    surface.monitor_name = state
                        .output_names
                        .iter()
                        .find(|(known, _)| known == output)
                        .map(|(_, name)| name.clone());
                    state.surfaces.push(surface);
                }
            }
            OutputSelection::Compositor => {
                state.surfaces.push(create_surface(
                    compositor,
                    layer_shell,
                    None,
                    surface_id,
                    config,
                    qh,
                ));
            }
            OutputSelection::Named(name) => {
                let output = state
                    .output_names
                    .iter()
                    .find(|(_, known_name)| known_name == name)
                    .map(|(output, _)| output.clone())
                    .ok_or_else(|| format!("Wayland output `{name}` is unavailable"))?;
                let mut surface = create_surface(
                    compositor,
                    layer_shell,
                    Some(&output),
                    surface_id,
                    config,
                    qh,
                );

                surface.monitor_name = Some(name.clone());
                state.surfaces.push(surface);
            }
        }
    }

    Ok(())
}

fn create_surface(
    compositor: &wl_compositor::WlCompositor,
    layer_shell: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
    output: Option<&wl_output::WlOutput>,
    surface_id: SurfaceId,
    config: &SurfaceConfig,
    qh: &QueueHandle<State>,
) -> SurfaceState {
    let surface = compositor.create_surface(qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        output,
        layer(config.layer),
        config.namespace.clone(),
        qh,
        (),
    );

    let anchors = anchors(config.anchors);

    if !config.anchors.is_empty() {
        layer_surface.set_anchor(anchors);
    }

    layer_surface.set_size(config.size.width, config.size.height);
    layer_surface.set_exclusive_zone(config.exclusive_zone);
    layer_surface.set_keyboard_interactivity(keyboard_interactivity(config.keyboard_interactivity));

    if config.position != super::Position::default() {
        let left =
            if config.anchors.contains(Anchors::LEFT) && !config.anchors.contains(Anchors::RIGHT) {
                config.position.x
            } else {
                0
            };

        let right =
            if config.anchors.contains(Anchors::RIGHT) && !config.anchors.contains(Anchors::LEFT) {
                config.position.x
            } else {
                0
            };

        layer_surface.set_margin(config.position.y, right, 0, left);
    }

    surface.commit();
    SurfaceState::new(surface_id, output.cloned(), surface, layer_surface, config)
}

fn layer(layer: Layer) -> zwlr_layer_shell_v1::Layer {
    match layer {
        Layer::Background => zwlr_layer_shell_v1::Layer::Background,
        Layer::Bottom => zwlr_layer_shell_v1::Layer::Bottom,
        Layer::Top => zwlr_layer_shell_v1::Layer::Top,
        Layer::Overlay => zwlr_layer_shell_v1::Layer::Overlay,
    }
}

fn anchors(value: Anchors) -> zwlr_layer_surface_v1::Anchor {
    let mut result = zwlr_layer_surface_v1::Anchor::empty();

    if value.contains(Anchors::TOP) {
        result |= zwlr_layer_surface_v1::Anchor::Top;
    }

    if value.contains(Anchors::BOTTOM) {
        result |= zwlr_layer_surface_v1::Anchor::Bottom;
    }

    if value.contains(Anchors::LEFT) {
        result |= zwlr_layer_surface_v1::Anchor::Left;
    }

    if value.contains(Anchors::RIGHT) {
        result |= zwlr_layer_surface_v1::Anchor::Right;
    }

    result
}

fn keyboard_interactivity(
    value: KeyboardInteractivity,
) -> zwlr_layer_surface_v1::KeyboardInteractivity {
    match value {
        KeyboardInteractivity::None => zwlr_layer_surface_v1::KeyboardInteractivity::None,
        KeyboardInteractivity::OnDemand => zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
        KeyboardInteractivity::Exclusive => zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
    }
}

struct State {
    shm: wl_shm::WlShm,
    _outputs: Vec<wl_output::WlOutput>,
    output_names: Vec<(wl_output::WlOutput, String)>,
    surfaces: Vec<SurfaceState>,
    _seat: wl_seat::WlSeat,
    _pointer: wl_pointer::WlPointer,
    _keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer_surface: Option<usize>,
    pointer_position: PositionF64,
    keyboard_surface: Option<usize>,
    pending_events: Vec<PendingEvent>,
    xkb: Option<xkb::State>,
    repeat_delay: Duration,
    repeat_interval: Option<Duration>,
    repeating_key: Option<RepeatingKey>,
    closed: bool,
}

struct SurfaceState {
    logical_surface: SurfaceId,
    output: Option<wl_output::WlOutput>,
    surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    buffer: Option<ShmBuffer>,
    frame_callback: Option<wl_callback::WlCallback>,
    size: Size,
    monitor_name: Option<String>,
    needs_redraw: bool,
    last_draw: Instant,
    last_pixels: Option<Vec<u8>>,
}

impl SurfaceState {
    fn new(
        logical_surface: SurfaceId,
        output: Option<wl_output::WlOutput>,
        surface: wl_surface::WlSurface,
        layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        config: &SurfaceConfig,
    ) -> Self {
        let size = Size::new(
            config.size.width.max(WIDTH_FALLBACK),
            config.size.height.max(HEIGHT_FALLBACK),
        );

        Self {
            logical_surface,
            output,
            surface,
            layer_surface,
            buffer: None,
            frame_callback: None,
            size,
            monitor_name: None,
            needs_redraw: true,
            last_draw: Instant::now(),
            last_pixels: None,
        }
    }
}

impl State {
    fn new(
        shm: wl_shm::WlShm,
        outputs: Vec<wl_output::WlOutput>,
        surfaces: Vec<SurfaceState>,
        seat: wl_seat::WlSeat,
        pointer: wl_pointer::WlPointer,
        keyboard: Option<wl_keyboard::WlKeyboard>,
    ) -> Self {
        Self {
            shm,
            _outputs: outputs,
            output_names: Vec::new(),

            surfaces,
            _seat: seat,
            _pointer: pointer,
            _keyboard: keyboard,
            pointer_surface: None,
            pointer_position: PositionF64::default(),
            keyboard_surface: None,
            pending_events: Vec::new(),
            xkb: None,
            repeat_delay: DEFAULT_REPEAT_DELAY,
            repeat_interval: Some(DEFAULT_REPEAT_INTERVAL),
            repeating_key: None,
            closed: false,
        }
    }

    fn mark_all_for_redraw(&mut self) {
        for surface in &mut self.surfaces {
            surface.needs_redraw = true;
        }
    }

    fn queue_key_input(&mut self, surface: usize, keycode: u32) {
        let Some(xkb) = &self.xkb else {
            return;
        };
        let code = xkb::Keycode::new(keycode + 8);
        let symbol = xkb.key_get_one_sym(code).raw();
        let text = xkb.key_get_utf8(code);

        self.pending_events.push(PendingEvent {
            surface: self.surfaces[surface].logical_surface,
            event: InputEvent::KeyInput {
                key: match symbol {
                    xkb::keysyms::KEY_Escape => Key::Escape,
                    xkb::keysyms::KEY_Return | xkb::keysyms::KEY_KP_Enter => Key::Return,
                    xkb::keysyms::KEY_BackSpace => Key::Backspace,
                    xkb::keysyms::KEY_Up => Key::Up,
                    xkb::keysyms::KEY_Down => Key::Down,
                    _ => Key::Other,
                },

                text: (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text),
            },
        });
    }

    fn repeat_key(&mut self) {
        let Some(interval) = self.repeat_interval else {
            self.repeating_key = None;
            return;
        };
        let Some(repeat) = &mut self.repeating_key else {
            return;
        };

        if Instant::now() < repeat.next {
            return;
        }

        let keycode = repeat.keycode;
        let surface = repeat.surface;
        repeat.next = Instant::now() + interval;
        self.queue_key_input(surface, keycode);
    }

    fn draw_pending(
        &mut self,
        qh: &QueueHandle<Self>,
        shell: &mut dyn Shell,
    ) -> Result<(), Box<dyn Error>> {
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].needs_redraw && self.surfaces[index].frame_callback.is_none() {
                self.draw(index, qh, shell)?;
            }
        }

        Ok(())
    }

    fn draw(
        &mut self,
        index: usize,
        qh: &QueueHandle<Self>,
        shell: &mut dyn Shell,
    ) -> Result<(), Box<dyn Error>> {
        let logical_surface = self.surfaces[index].logical_surface;
        let size = self.surfaces[index].size;
        let monitor_name = self.surfaces[index].monitor_name.clone();
        let pixels = shell.render(logical_surface, size, monitor_name.as_deref())?;
        let expected = size.width as usize * size.height as usize * 4;

        if pixels.len() != expected {
            return Err(format!(
                "surface {:?} rendered {} bytes, expected {expected}",
                logical_surface,
                pixels.len()
            )
            .into());
        }

        let stride = size.width * 4;

        if self.surfaces[index].last_pixels.as_ref() == Some(&pixels) {
            let surface = &mut self.surfaces[index];
            surface.frame_callback = Some(surface.surface.frame(qh, ()));
            surface.surface.damage_buffer(0, 0, 1, 1);
            surface.surface.commit();
            surface.needs_redraw = false;
            surface.last_draw = Instant::now();
            return Ok(());
        }

        let buffer_size = stride * size.height;
        let file = tempfile::tempfile()?;
        file.set_len(u64::from(buffer_size))?;
        // SAFETY: This private file has the requested length and is never truncated.
        // The compositor only reads the mapping after the buffer is committed.
        let mut mapping = unsafe { MmapMut::map_mut(&file)? };
        mapping.copy_from_slice(&pixels);
        let pool = self
            .shm
            .create_pool(file.as_fd(), i32::try_from(buffer_size)?, qh, ());
        let buffer = pool.create_buffer(
            0,
            i32::try_from(size.width)?,
            i32::try_from(size.height)?,
            i32::try_from(stride)?,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );

        pool.destroy();
        let surface = &mut self.surfaces[index];
        surface.surface.attach(Some(&buffer), 0, 0);
        surface.surface.damage_buffer(
            0,
            0,
            i32::try_from(size.width)?,
            i32::try_from(size.height)?,
        );
        surface.frame_callback = Some(surface.surface.frame(qh, ()));
        surface.surface.commit();
        surface.buffer = Some(ShmBuffer {
            _buffer: buffer,
            _mapping: Arc::new(mapping),
        });
        surface.needs_redraw = false;
        surface.last_draw = Instant::now();
        surface.last_pixels = Some(pixels);
        Ok(())
    }

    fn surface_index(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|state| &state.surface == surface)
    }
}

struct ShmBuffer {
    _buffer: wl_buffer::WlBuffer,
    _mapping: Arc<MmapMut>,
}

struct PendingEvent {
    surface: SurfaceId,
    event: InputEvent,
}

fn handle_layer_configure(
    state: &mut State,
    proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    serial: u32,
    width: u32,
    height: u32,
) {
    let Some(surface) = state
        .surfaces
        .iter_mut()
        .find(|surface| &surface.layer_surface == proxy)
    else {
        return;
    };

    surface.layer_surface.ack_configure(serial);

    if width > 0 {
        surface.size.width = width;
    }

    if height > 0 {
        surface.size.height = height;
    }

    surface.needs_redraw = true;

    state.pending_events.push(PendingEvent {
        surface: surface.logical_surface,
        event: InputEvent::Resized { size: surface.size },
    });
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => handle_layer_configure(state, proxy, serial, width, height),
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(surface) = state
                    .surfaces
                    .iter()
                    .find(|surface| &surface.layer_surface == proxy)
                {
                    state.pending_events.push(PendingEvent {
                        surface: surface.logical_surface,
                        event: InputEvent::CloseRequested,
                    });
                }

                state.closed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        qh: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. })
            && let Some(surface) = state
                .surfaces
                .iter_mut()
                .find(|surface| surface.frame_callback.as_ref() == Some(proxy))
        {
            surface.frame_callback = None;

            if surface.last_draw.elapsed() < REDRAW_INTERVAL && !surface.needs_redraw {
                surface.frame_callback = Some(surface.surface.frame(qh, ()));
                surface.surface.damage_buffer(0, 0, 1, 1);
                surface.surface.commit();
            } else {
                surface.needs_redraw = true;
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_surface = state.surface_index(&surface);

                state.pointer_position = PositionF64::new(surface_x, surface_y);

                if let Some(index) = state.pointer_surface {
                    state.pending_events.push(PendingEvent {
                        surface: state.surfaces[index].logical_surface,
                        event: InputEvent::PointerEnter {
                            position: state.pointer_position,
                            output: state.surfaces[index].monitor_name.clone(),
                        },
                    });
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if let Some(index) = state.surface_index(&surface) {
                    state.pending_events.push(PendingEvent {
                        surface: state.surfaces[index].logical_surface,
                        event: InputEvent::PointerLeave,
                    });
                }

                state.pointer_surface = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_position = PositionF64::new(surface_x, surface_y);

                if let Some(index) = state.pointer_surface {
                    state.pending_events.push(PendingEvent {
                        surface: state.surfaces[index].logical_surface,
                        event: InputEvent::PointerMotion {
                            position: state.pointer_position,
                            output: state.surfaces[index].monitor_name.clone(),
                        },
                    });
                }
            }
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => {
                if let Some(index) = state.pointer_surface {
                    state.pending_events.push(PendingEvent {
                        surface: state.surfaces[index].logical_surface,
                        event: InputEvent::PointerButton {
                            position: state.pointer_position,
                            button: mouse_button(button),
                            pressed: button_state.into_result().ok()
                                == Some(wl_pointer::ButtonState::Pressed),

                            output: state.surfaces[index].monitor_name.clone(),
                        },
                    });
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if let Some(index) = state.pointer_surface {
                    let (delta_x, delta_y) = match axis {
                        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => (value, 0.0),
                        WEnum::Value(wl_pointer::Axis::VerticalScroll) => (0.0, value),
                        _ => (0.0, 0.0),
                    };

                    state.pending_events.push(PendingEvent {
                        surface: state.surfaces[index].logical_surface,
                        event: InputEvent::PointerScroll {
                            position: state.pointer_position,
                            delta_x,
                            delta_y,
                            output: state.surfaces[index].monitor_name.clone(),
                        },
                    });
                }
            }
            _ => {}
        }
    }
}

fn handle_keyboard_key(state: &mut State, key: u32, pressed: bool) {
    let Some(index) = state.keyboard_surface else {
        return;
    };
    state.pending_events.push(PendingEvent {
        surface: state.surfaces[index].logical_surface,
        event: InputEvent::Key {
            keycode: key,
            pressed,
        },
    });

    if pressed {
        state.queue_key_input(index, key);

        let key_repeats = state.repeat_interval.is_some()
            && state
                .xkb
                .as_ref()
                .is_some_and(|xkb| xkb.get_keymap().key_repeats(xkb::Keycode::new(key + 8)));

        if key_repeats {
            state.repeating_key = Some(RepeatingKey {
                keycode: key,
                surface: index,
                next: Instant::now() + state.repeat_delay,
            });
        }
    } else if state
        .repeating_key
        .as_ref()
        .is_some_and(|repeat| repeat.keycode == key && repeat.surface == index)
    {
        state.repeating_key = None;
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                state.repeating_key = None;

                if format.into_result().ok() == Some(wl_keyboard::KeymapFormat::XkbV1) {
                    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);

                    // SAFETY: Wayland supplies an owned keymap fd and its mapping length;
                    // xkbcommon maps it read-only and does not retain the descriptor.
                    match unsafe {
                        xkb::Keymap::new_from_fd(
                            &context,
                            fd,
                            size as usize,
                            xkb::KEYMAP_FORMAT_TEXT_V1,
                            xkb::KEYMAP_COMPILE_NO_FLAGS,
                        )
                    } {
                        Ok(Some(keymap)) => state.xkb = Some(xkb::State::new(&keymap)),
                        _ => state.xkb = None,
                    }
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = &mut state.xkb {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Enter { surface, .. } => {
                state.repeating_key = None;
                state.keyboard_surface = state.surface_index(&surface);
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                state.repeating_key = None;

                if let Some(index) = state.surface_index(&surface) {
                    state.pending_events.push(PendingEvent {
                        surface: state.surfaces[index].logical_surface,
                        event: InputEvent::FocusLost,
                    });
                }

                state.keyboard_surface = None;
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.repeat_interval = (rate > 0).then(|| {
                    Duration::from_secs_f64(1.0 / f64::from(rate)).max(Duration::from_millis(1))
                });
                state.repeat_delay = Duration::from_millis(u64::from(delay.max(0).cast_unsigned()));

                state.repeating_key = None;
            }
            wl_keyboard::Event::Key {
                key, state: xkb, ..
            } => {
                let pressed = xkb.into_result().ok() == Some(wl_keyboard::KeyState::Pressed);
                handle_keyboard_key(state, key, pressed);
            }
            _ => {}
        }
    }
}

fn mouse_button(button: u32) -> MouseButton {
    match button {
        0x110 => MouseButton::Left,
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        other => MouseButton::Other(other),
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some((_, known_name)) = state
                .output_names
                .iter_mut()
                .find(|(known, _)| known == proxy)
            {
                known_name.clone_from(&name);
            } else {
                state.output_names.push((proxy.clone(), name.clone()));
            }

            for surface in state
                .surfaces
                .iter_mut()
                .filter(|surface| surface.output.as_ref() == Some(proxy))
            {
                surface.monitor_name = Some(name.clone());
                surface.needs_redraw = true;
            }
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
