//! Direct `wgpu` + Slug renderer shared by the native desktop and WebGL2 demo runners.

mod crt;
mod renderer;
mod slug;

pub use apus::grid::{Cell, CellGrid, CellMetrics, GridBackend};
pub use renderer::Renderer;
