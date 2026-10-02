//! Window-system backend implementations.

#[cfg(feature = "wayland")]
pub mod wayland;
#[cfg(feature = "x11")]
pub mod x11;

pub use crate::{
    Anchors, Backend, InputEvent, KeyboardInteractivity, Layer, MouseButton, OutputSelection,
    Position, PositionF64, Shell, Size, SurfaceConfig, SurfaceId,
};
