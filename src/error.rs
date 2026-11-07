use std::error::Error as StdError;

use crate::SurfaceId;

/// A surface rendering or window-system failure with its original cause.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A window-system backend failed during setup or event processing.
    #[error("{source}")]
    Backend {
        backend: &'static str,
        #[source]
        source: Box<dyn StdError>,
    },
    /// The application's renderer could not produce a frame for a surface.
    #[error("{source}")]
    Render {
        surface: SurfaceId,
        #[source]
        source: Box<dyn StdError>,
    },
}
