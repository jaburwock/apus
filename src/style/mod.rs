//! Shared styling façade for the application UI.
//!
//! The module exposes semantic palette namespaces at the root so viewers can request colours by
//! role without knowing palette storage or ramp details. `palette` owns colour lookup and runtime
//! theme state, and `pane` owns the app-level palette editor.
//!
//! This is a UI vocabulary and ownership boundary, not a viewer abstraction: it contains no
//! genome or viewer state and remains a leaf that consumers can import without creating a
//! dependency back into the application architecture.

pub mod palette;
pub mod pane;

pub use palette::{
    Alignment, Chrome, Coordinate, Coverage, Features, Indel, LocalArea, Minimap, Reference, Selection, Status,
    Surface, Text,
};
