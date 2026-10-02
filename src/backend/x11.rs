use std::{error::Error, thread, time::Duration};

use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::xproto::{
        AtomEnum, ClientMessageEvent, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask,
        ImageFormat, InputFocus, PropMode, WindowClass,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as WrapperConnectionExt,
};

use super::{
    Backend, InputEvent, KeyboardInteractivity, Layer, MouseButton, PositionF64, Shell, Size,
    SurfaceConfig, SurfaceId,
};

/// X11 backend for ordinary GUI windows and dock/panel-style surfaces.
#[derive(Default)]
pub struct X11Backend;

impl Backend for X11Backend {
    fn run(&mut self, shell: &mut dyn Shell) -> Result<(), Box<dyn Error>> {
        let configs = shell.surface_configs().to_vec();
        if configs.is_empty() {
            return Err("shell has no surfaces".into());
        }

        let (connection, screen_number) = RustConnection::connect(None)?;
        let screen = &connection.setup().roots[screen_number];
        let atoms = Atoms::new(&connection)?.reply()?;
        let mut windows = Vec::with_capacity(configs.len());

        for (index, config) in configs.iter().enumerate() {
            let window = connection.generate_id()?;
            let width = config.size.width.max(1);
            let height = config.size.height.max(1);
            let event_mask = EventMask::EXPOSURE
                | EventMask::STRUCTURE_NOTIFY
                | EventMask::POINTER_MOTION
                | EventMask::ENTER_WINDOW
                | EventMask::LEAVE_WINDOW
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::KEY_PRESS
                | EventMask::KEY_RELEASE;

            connection.create_window(
                0,
                window,
                screen.root,
                config.position.x as i16,
                config.position.y as i16,
                width as u16,
                height as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .background_pixel(screen.black_pixel)
                    .override_redirect(u32::from(config.override_redirect))
                    .event_mask(event_mask),
            )?;

            connection.change_property8(
                PropMode::REPLACE,
                window,
                atoms.net_wm_name,
                AtomEnum::STRING,
                config.namespace.as_bytes(),
            )?;
            connection.change_property32(
                PropMode::REPLACE,
                window,
                atoms.wm_protocols,
                AtomEnum::ATOM,
                &[atoms.wm_delete_window],
            )?;

            let is_panel = !config.anchors.is_empty() || config.exclusive_zone > 0;
            let window_type = if is_panel {
                atoms.net_wm_window_type_dock
            } else {
                atoms.net_wm_window_type_normal
            };
            connection.change_property32(
                PropMode::REPLACE,
                window,
                atoms.net_wm_window_type,
                AtomEnum::ATOM,
                &[window_type],
            )?;
            if matches!(config.layer, Layer::Top | Layer::Overlay) {
                connection.change_property32(
                    PropMode::REPLACE,
                    window,
                    atoms.net_wm_state,
                    AtomEnum::ATOM,
                    &[atoms.net_wm_state_above],
                )?;
            }
            set_strut(&connection, window, atoms, screen, config)?;
            connection.map_window(window)?;
            if config.keyboard_interactivity != KeyboardInteractivity::None {
                connection.set_input_focus(InputFocus::POINTER_ROOT, window, CURRENT_TIME)?;
            }
            let gc = connection.generate_id()?;
            connection.create_gc(gc, window, &CreateGCAux::new())?;
            windows.push(WindowState {
                surface: SurfaceId(index),
                window,
                gc,
                size: Size::new(width, height),
            });
        }
        connection.flush()?;

        let mut state = State {
            connection,
            atoms,
            windows,
            shell,
        };
        for index in 0..state.windows.len() {
            state.redraw(index)?;
        }

        loop {
            while let Some(event) = state.connection.poll_for_event()? {
                if !state.handle_event(event)? {
                    return Ok(());
                }
            }
            if state.shell.take_redraw_request() {
                for index in 0..state.windows.len() {
                    state.redraw(index)?;
                }
            }
            // A modest timer keeps animated or time-based surfaces current
            // without making the backend depend on a widget implementation.
            thread::sleep(Duration::from_millis(50));
        }
    }
}

struct WindowState {
    surface: SurfaceId,
    window: u32,
    gc: u32,
    size: Size,
}

struct State<'a> {
    connection: RustConnection,
    atoms: Atoms,
    windows: Vec<WindowState>,
    shell: &'a mut dyn Shell,
}

impl State<'_> {
    fn redraw(&mut self, index: usize) -> Result<(), Box<dyn Error>> {
        let window = &self.windows[index];
        let pixels = self.shell.render(window.surface, window.size, None)?;
        let expected = window.size.width as usize * window.size.height as usize * 4;
        if pixels.len() != expected {
            return Err(format!(
                "surface {:?} rendered {} bytes, expected {expected}",
                window.surface,
                pixels.len()
            )
            .into());
        }
        self.connection.put_image(
            ImageFormat::Z_PIXMAP,
            window.window,
            window.gc,
            window.size.width as u16,
            window.size.height as u16,
            0,
            0,
            0,
            24,
            &pixels,
        )?;
        self.connection.flush()?;
        Ok(())
    }

    fn window_index(&self, window: u32) -> Option<usize> {
        self.windows.iter().position(|state| state.window == window)
    }

    fn handle_event(&mut self, event: x11rb::protocol::Event) -> Result<bool, Box<dyn Error>> {
        use x11rb::protocol::Event;

        match event {
            Event::Expose(event) => {
                if let Some(index) = self.window_index(event.window) {
                    self.redraw(index)?;
                }
            }
            Event::ConfigureNotify(event) => {
                if let Some(index) = self.window_index(event.window) {
                    let size = Size::new(
                        u32::from(event.width).max(1),
                        u32::from(event.height).max(1),
                    );
                    self.windows[index].size = size;
                    let surface = self.windows[index].surface;
                    self.shell
                        .handle_event(surface, InputEvent::Resized { size });
                    self.redraw(index)?;
                }
            }
            Event::EnterNotify(event) => {
                self.pointer_event(
                    event.event,
                    InputEvent::PointerEnter {
                        position: PositionF64::new(
                            f64::from(event.event_x),
                            f64::from(event.event_y),
                        ),
                    },
                );
            }
            Event::LeaveNotify(event) => {
                self.pointer_event(event.event, InputEvent::PointerLeave);
            }
            Event::MotionNotify(event) => {
                self.pointer_event(
                    event.event,
                    InputEvent::PointerMotion {
                        position: PositionF64::new(
                            f64::from(event.event_x),
                            f64::from(event.event_y),
                        ),
                    },
                );
            }
            Event::ButtonPress(event) => {
                if let Some(index) = self.window_index(event.event) {
                    let position =
                        PositionF64::new(f64::from(event.event_x), f64::from(event.event_y));
                    let input = match event.detail {
                        4 => Some(InputEvent::PointerScroll {
                            position,
                            delta_x: 0.0,
                            delta_y: -1.0,
                        }),
                        5 => Some(InputEvent::PointerScroll {
                            position,
                            delta_x: 0.0,
                            delta_y: 1.0,
                        }),
                        6 => Some(InputEvent::PointerScroll {
                            position,
                            delta_x: 1.0,
                            delta_y: 0.0,
                        }),
                        7 => Some(InputEvent::PointerScroll {
                            position,
                            delta_x: -1.0,
                            delta_y: 0.0,
                        }),
                        button => Some(InputEvent::PointerButton {
                            position,
                            button: mouse_button(button),
                            pressed: true,
                        }),
                    };
                    self.shell
                        .handle_event(self.windows[index].surface, input.unwrap());
                    self.redraw(index)?;
                }
            }
            Event::ButtonRelease(event) if event.detail <= 3 => {
                self.pointer_event_with_position(
                    event.event,
                    PositionF64::new(f64::from(event.event_x), f64::from(event.event_y)),
                    InputEvent::PointerButton {
                        position: PositionF64::new(
                            f64::from(event.event_x),
                            f64::from(event.event_y),
                        ),
                        button: mouse_button(event.detail),
                        pressed: false,
                    },
                );
            }
            Event::KeyPress(event) => self.key_event(event.event, event.detail, true),
            Event::KeyRelease(event) => self.key_event(event.event, event.detail, false),
            Event::ClientMessage(ClientMessageEvent { window, data, .. })
                if data.as_data32()[0] == self.atoms.wm_delete_window =>
            {
                if let Some(index) = self.window_index(window) {
                    self.shell
                        .handle_event(self.windows[index].surface, InputEvent::CloseRequested);
                }
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    }

    fn pointer_event(&mut self, window: u32, event: InputEvent) {
        if let Some(index) = self.window_index(window) {
            self.shell.handle_event(self.windows[index].surface, event);
        }
    }

    fn pointer_event_with_position(
        &mut self,
        window: u32,
        _position: PositionF64,
        event: InputEvent,
    ) {
        self.pointer_event(window, event);
    }

    fn key_event(&mut self, window: u32, keycode: u8, pressed: bool) {
        if let Some(index) = self.window_index(window) {
            self.shell.handle_event(
                self.windows[index].surface,
                InputEvent::Key {
                    keycode: u32::from(keycode),
                    pressed,
                },
            );
        }
    }
}

fn mouse_button(button: u8) -> MouseButton {
    match button {
        1 => MouseButton::Left,
        2 => MouseButton::Middle,
        3 => MouseButton::Right,
        other => MouseButton::Other(u32::from(other)),
    }
}

fn set_strut(
    connection: &RustConnection,
    window: u32,
    atoms: Atoms,
    screen: &x11rb::protocol::xproto::Screen,
    config: &SurfaceConfig,
) -> Result<(), Box<dyn Error>> {
    let zone = u32::try_from(config.exclusive_zone.max(0)).unwrap_or(0);
    if zone == 0 {
        return Ok(());
    }
    let width = u32::from(screen.width_in_pixels);
    let height = u32::from(screen.height_in_pixels);
    let top = if config.anchors.contains(super::Anchors::TOP) {
        zone
    } else {
        0
    };
    let bottom = if config.anchors.contains(super::Anchors::BOTTOM) {
        zone
    } else {
        0
    };
    let left = if config.anchors.contains(super::Anchors::LEFT) {
        zone
    } else {
        0
    };
    let right = if config.anchors.contains(super::Anchors::RIGHT) {
        zone
    } else {
        0
    };
    connection.change_property32(
        PropMode::REPLACE,
        window,
        atoms.net_wm_strut,
        AtomEnum::CARDINAL,
        &[left, right, top, bottom],
    )?;
    connection.change_property32(
        PropMode::REPLACE,
        window,
        atoms.net_wm_strut_partial,
        AtomEnum::CARDINAL,
        &[
            left,
            right,
            top,
            bottom,
            0,
            height.saturating_sub(1),
            0,
            height.saturating_sub(1),
            0,
            width.saturating_sub(1),
            0,
            width.saturating_sub(1),
        ],
    )?;
    Ok(())
}

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        net_wm_window_type,
        net_wm_window_type_dock,
        net_wm_window_type_normal,
        net_wm_state,
        net_wm_state_above,
        net_wm_strut,
        net_wm_strut_partial,
        net_wm_name,
        wm_protocols,
        wm_delete_window,
    }
}
