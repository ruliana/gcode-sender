//! The parts of the sender that do not touch the serial port or the network:
//! the G-code interpreter, the controller's reply parser, work offsets,
//! boundaries, settings, and the job store.
//!
//! They live in a library so the property tests and the fuzz targets can
//! reach them. The binary, `main.rs`, builds the rest on top of them.

pub mod bounds;
pub mod gcode;
pub mod grbl;
pub mod position;
pub mod program;
pub mod settings;
pub mod store;
pub mod work;
