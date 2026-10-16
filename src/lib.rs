//! Shared library for the `fvalidate` CLI and the optional `fview` GUI.
//!
//! The `fvalidate` binary has its own module tree (so it keeps building with no
//! GUI dependency); this library exposes the same modules so the optional GUI
//! can evaluate a rule set against the file it has open.

pub mod compare;
pub mod dsl;
pub mod engine;
pub mod expr;
pub mod mapping;
pub mod pattern;
pub mod progress;
pub mod report;
pub mod rules;
pub mod sampler;
pub mod transform;
