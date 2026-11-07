//! A small, renderer-agnostic GUI shell library for Wayland layer-shell and X11.
//!
//! Applications describe one or more surfaces with [`SurfaceConfig`], provide
//! pixels and input handling through [`Shell`], and then run a backend from
//! [`backend`]. The crate intentionally has no widget, layout, or text
//! rendering dependency.

pub mod backend;
mod error;

use std::ops::{BitOr, BitOrAssign};

pub use crate::error::Error;

/// An index identifying one entry returned by [`Shell::surface_configs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub usize);

/// A two-dimensional size in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    /// Creates a physical pixel size; zero dimensions request compositor sizing.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// A two-dimensional position in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

impl Position {
    /// Creates a position in physical pixels, including negative coordinates.
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A layer-shell surface's z-order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layer {
    Background,
    Bottom,
    #[default]
    Top,
    Overlay,
}

/// Keyboard focus policy for a layer-shell surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyboardInteractivity {
    #[default]
    None,
    OnDemand,
    Exclusive,
}

/// Which output a layer-shell surface is attached to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum OutputSelection {
    /// Create one surface on every advertised output.
    All,
    /// Let the compositor choose the output for one surface.
    #[default]
    Compositor,
    /// Create one surface on the output with this `wl_output` name.
    Named(String),
}

/// Edge anchors used by layer-shell. Empty anchors describe a floating surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Anchors(u8);

impl Anchors {
    pub const TOP: Self = Self(1 << 0);

    pub const BOTTOM: Self = Self(1 << 1);

    pub const LEFT: Self = Self(1 << 2);

    pub const RIGHT: Self = Self(1 << 3);

    /// Returns an anchor set with no edges selected.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns whether no edges are selected.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns whether every edge in `other` is selected.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Anchors {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Anchors {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Window and layer-shell settings for one logical surface.
#[derive(Clone, Debug)]
pub struct SurfaceConfig {
    /// Layer-shell namespace and the X11 window title.
    pub namespace: String,
    /// Requested size. A zero dimension lets the compositor choose it.
    pub size: Size,
    /// X11 position. Layer-shell compositors position surfaces from anchors.
    pub position: Position,
    pub layer: Layer,
    pub anchors: Anchors,
    /// `0` means no exclusive reservation; positive values reserve that many
    /// pixels on the anchored edge for compatible compositors/window managers.
    pub exclusive_zone: i32,
    pub keyboard_interactivity: KeyboardInteractivity,
    pub output: OutputSelection,
    /// Whether an X11 window bypasses the window manager.
    pub override_redirect: bool,
}

impl SurfaceConfig {
    /// Creates a top-layer surface with compositor-selected output and no anchors.
    pub fn new(namespace: impl Into<String>, size: Size) -> Self {
        Self {
            namespace: namespace.into(),
            size,
            position: Position::default(),
            layer: Layer::Top,
            anchors: Anchors::empty(),
            exclusive_zone: 0,
            keyboard_interactivity: KeyboardInteractivity::None,
            output: OutputSelection::Compositor,
            override_redirect: false,
        }
    }
}

/// A normalized pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Other(u32),
}

/// A normalized input event delivered to a logical surface.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    CloseRequested,
    FocusLost,
    Resized {
        size: Size,
    },
    PointerEnter {
        position: PositionF64,
        output: Option<String>,
    },
    PointerLeave,
    PointerMotion {
        position: PositionF64,
        output: Option<String>,
    },

    PointerButton {
        position: PositionF64,
        button: MouseButton,
        pressed: bool,
        output: Option<String>,
    },
    PointerScroll {
        position: PositionF64,
        delta_x: f64,
        delta_y: f64,
        output: Option<String>,
    },
    Key {
        keycode: u32,
        pressed: bool,
    },

    /// A resolved key press. `text` is absent for navigation or modifier keys.
    KeyInput {
        key: Key,
        text: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Escape,
    Return,
    Backspace,
    Up,
    Down,
    Other,
}

/// A pointer position in surface-local coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PositionF64 {
    pub x: f64,
    pub y: f64,
}

impl PositionF64 {
    /// Creates a surface-local pointer position in physical pixels.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Application callbacks consumed by a window-system backend.
pub trait Shell {
    /// Return the logical surfaces that should be created at startup.
    fn surface_configs(&self) -> &[SurfaceConfig];

    /// Render one surface into exactly `size.width * size.height * 4` bytes
    /// of 32-bit ARGB8888 pixels in native byte order.
    /// On the little-endian Linux systems supported by the current backends,
    /// this is B, G, R, A for each pixel.
    ///
    /// # Errors
    /// Returns [`Error::Render`] with this surface and the application cause if rendering fails.
    fn render(
        &mut self,
        surface: SurfaceId,
        size: Size,
        output: Option<&str>,
    ) -> Result<Vec<u8>, Error>;

    /// Handle an input event belonging to one logical surface.
    fn handle_event(&mut self, surface: SurfaceId, event: InputEvent);

    /// Return whether the application needs another frame.
    fn take_redraw_request(&mut self) -> bool {
        false
    }

    /// Return true to close the surfaces and stop the backend event loop.
    fn should_close(&self) -> bool {
        false
    }
}

/// A window-system backend that owns native windows and its event loop.
pub trait Backend {
    /// Runs until the shell or window system requests closure.
    ///
    /// # Errors
    /// Returns [`Error::Backend`] with the backend name and original cause if setup,
    /// event processing, or rendering fails. Rendering failures retain [`Error::Render`]
    /// in the source chain.
    fn run(&mut self, shell: &mut dyn Shell) -> Result<(), Error>;
}
