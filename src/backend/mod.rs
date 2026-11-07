//! Window-system backend implementations.

#[cfg(feature = "wayland")]
pub mod wayland;
#[cfg(feature = "x11")]
pub mod x11;

pub use crate::{
    Anchors, Backend, InputEvent, Key, KeyboardInteractivity, Layer, MouseButton, OutputSelection,
    Position, PositionF64, Shell, Size, SurfaceConfig, SurfaceId,
};

#[cfg(all(test, any(feature = "wayland", feature = "x11")))]
mod tests {
    use std::error::Error as _;

    use crate::{Backend, Error, InputEvent, Shell, Size, SurfaceConfig, SurfaceId};

    struct EmptyShell;

    impl Shell for EmptyShell {
        fn surface_configs(&self) -> &[SurfaceConfig] {
            &[]
        }

        fn render(
            &mut self,
            _surface: SurfaceId,
            _size: Size,
            _output: Option<&str>,
        ) -> Result<Vec<u8>, Error> {
            unreachable!("an empty shell has no renderable surfaces")
        }

        fn handle_event(&mut self, _surface: SurfaceId, _event: InputEvent) {}
    }

    fn rejects_empty_shell(backend: &mut dyn Backend, name: &str) {
        let error = backend.run(&mut EmptyShell).unwrap_err();
        assert!(matches!(&error, Error::Backend { backend, .. } if *backend == name));
        assert_eq!(error.source().unwrap().to_string(), "shell has no surfaces");
    }

    #[cfg(feature = "wayland")]
    #[test]
    fn wayland_rejects_empty_shell_before_connecting() {
        rejects_empty_shell(&mut super::wayland::WaylandBackend, "Wayland");
    }

    #[cfg(feature = "x11")]
    #[test]
    fn x11_rejects_empty_shell_before_connecting() {
        rejects_empty_shell(&mut super::x11::X11Backend, "X11");
    }
}
