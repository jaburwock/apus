//! Ratatui cell backend and wgpu renderer shared by the desktop and web runners.

mod backend;
mod color;
mod crt;
mod grid;
mod procedural;
mod renderer;
mod slug;

pub use backend::GridBackend;
pub use grid::{Cell, CellGrid};
pub use renderer::Renderer;
pub use slug::CellMetrics;
