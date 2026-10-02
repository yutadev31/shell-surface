use std::{
    error::Error,
    os::fd::AsFd,
    sync::Arc,
    time::{Duration, Instant},
};

use memmap2::MmapMut;
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    globals::GlobalListContents,
    globals::registry_queue_init,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
        wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::{
    Anchors, Backend, InputEvent, KeyboardInteractivity, Layer, MouseButton, OutputSelection,
    PositionF64, Shell, Size, SurfaceConfig, SurfaceId,
};

const WIDTH_FALLBACK: u32 = 1280;
const HEIGHT_FALLBACK: u32 = 1;
const REDRAW_INTERVAL: Duration = Duration::from_millis(16);

/// Wayland layer-shell backend for panels, notifications, launchers and other
/// compositor-managed GUI surfaces.
#[derive(Default)]
pub struct WaylandBackend;

impl Backend for WaylandBackend {
    fn run(&mut self, shell: &mut dyn Shell) -> Result<(), Box<dyn Error>> {
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

        let needs_output_binding = configs
            .iter()
            .any(|config| config.output == OutputSelection::All);
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

        let mut surfaces = Vec::new();
        for (index, config) in configs.iter().enumerate() {
            let surface_id = SurfaceId(index);
            match config.output {
                OutputSelection::All => {
                    for output in &outputs {
                        surfaces.push(create_surface(
                            &compositor,
                            &layer_shell,
                            Some(output),
                            surface_id,
                            config,
                            &qh,
                        ));
                    }
                }
                OutputSelection::Compositor => {
                    surfaces.push(create_surface(
                        &compositor,
                        &layer_shell,
                        None,
                        surface_id,
                        config,
                        &qh,
                    ));
                }
            }
        }

        let pointer = seat.get_pointer(&qh, ());
        let keyboard = configs
            .iter()
            .any(|config| config.keyboard_interactivity != KeyboardInteractivity::None)
            .then(|| seat.get_keyboard(&qh, ()));
        let mut state = State::new(shm, outputs, surfaces, seat, pointer, keyboard);
        event_queue.roundtrip(&mut state)?;
        state.draw_pending(&qh, shell)?;

        while !state.closed {
            event_queue.blocking_dispatch(&mut state)?;
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
            state.draw_pending(&qh, shell)?;
        }
        Ok(())
    }
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
        layer_surface.set_margin(config.position.y, 0, config.position.x, 0);
    }
    surface.commit();
    SurfaceState::new(
        surface_id,
        output.cloned(),
        surface,
        layer_surface,
        config.clone(),
    )
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
    surfaces: Vec<SurfaceState>,
    _seat: wl_seat::WlSeat,
    _pointer: wl_pointer::WlPointer,
    _keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer_surface: Option<usize>,
    pointer_position: PositionF64,
    keyboard_surface: Option<usize>,
    pending_events: Vec<PendingEvent>,
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
        config: SurfaceConfig,
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
            surfaces,
            _seat: seat,
            _pointer: pointer,
            _keyboard: keyboard,
            pointer_surface: None,
            pointer_position: PositionF64::default(),
            keyboard_surface: None,
            pending_events: Vec::new(),
            closed: false,
        }
    }

    fn mark_all_for_redraw(&mut self) {
        for surface in &mut self.surfaces {
            surface.needs_redraw = true;
        }
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
        file.set_len(buffer_size as u64)?;
        let mut mapping = unsafe { MmapMut::map_mut(&file)? };
        mapping.copy_from_slice(&pixels);
        let pool = self
            .shm
            .create_pool(file.as_fd(), buffer_size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            size.width as i32,
            size.height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        pool.destroy();
        let surface = &mut self.surfaces[index];
        surface.surface.attach(Some(&buffer), 0, 0);
        surface
            .surface
            .damage_buffer(0, 0, size.width as i32, size.height as i32);
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
            } => {
                if let Some(index) = state
                    .surfaces
                    .iter()
                    .position(|surface| &surface.layer_surface == proxy)
                {
                    let surface = &mut state.surfaces[index];
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
            }
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
            wl_keyboard::Event::Enter { surface, .. } => {
                state.keyboard_surface = state.surface_index(&surface);
            }
            wl_keyboard::Event::Leave { .. } => state.keyboard_surface = None,
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                if let Some(index) = state.keyboard_surface {
                    state.pending_events.push(PendingEvent {
                        surface: state.surfaces[index].logical_surface,
                        event: InputEvent::Key {
                            keycode: key,
                            pressed: key_state.into_result().ok()
                                == Some(wl_keyboard::KeyState::Pressed),
                        },
                    });
                }
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
        if let wl_output::Event::Name { name } = event
            && let Some(surface) = state
                .surfaces
                .iter_mut()
                .find(|surface| surface.output.as_ref() == Some(proxy))
        {
            surface.monitor_name = Some(name);
            surface.needs_redraw = true;
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
