mod dispatcher;
mod display;
mod platform;
#[cfg(test)]
mod text_system;
mod window;

pub use dispatcher::*;
pub(crate) use display::*;
pub(crate) use platform::*;
#[cfg(test)]
pub(crate) use text_system::*;
pub(crate) use window::*;
pub use window::{CompositionFrame, NativeSurfaceEvent, RecordedLayer};

pub use platform::{TestScreenCaptureSource, TestScreenCaptureStream};
