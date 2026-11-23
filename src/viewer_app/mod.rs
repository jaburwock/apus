//! Concrete, independently isolated implementations of the application's viewers.
//!
//! The Genome and VCF modules implement the root viewer contract with state, command
//! handling, rendering, input, and work ownership kept inside each viewer. The shell owns the
//! viewer collection and active tab; this module is not a registry or shared state layer.
//!
//! The remaining modules provide viewer-local rendering and viewport support. Commands that
//! coordinate viewers go through the router as typed outcomes.

mod bam_index;
mod bam_switcher;
pub(crate) mod framebuffer;
pub(crate) mod painter;
pub(crate) mod viewport;

pub(crate) mod genome;
pub(crate) mod vcf;
