//! # volsa2-core
//!
//! Protocol and audio processing library for KORG Volca Sample 2.
//!
//! This library is derived from [volsa2-cli](https://github.com/00nktk/volsa2)
//! by 00nktk, reorganized as a reusable library crate with platform-independent
//! components.
//!
//! ## Modules
//!
//! - `proto` - SysEx protocol message encoding/decoding
//! - `seven_bit` - 7-bit MIDI-safe data encoding
//! - `audio` - WAV file conversion and resampling
//! - `util` - Helper utilities
//!
//! ## Example
//!
//! ```rust,no_run
//! use volsa2_core::proto::SampleHeader;
//!
//! let header = SampleHeader {
//!     sample_no: 0,
//!     name: "Kick".to_string(),
//!     length: 44100,
//!     level: 100,
//!     speed: 0,
//! };
//! ```

pub mod proto;
pub mod seven_bit;
pub mod audio;
pub mod util;

// Re-export commonly used types
pub use proto::{
    SampleHeader, SampleData, SampleHeaderDumpRequest, SampleDataDumpRequest,
    SampleSpaceDump, SampleSpaceDumpRequest,
    SearchDeviceRequest, SearchDeviceReply, Status,
    Message, Incoming, Outgoing,
};
pub use seven_bit::{U7, U7ToU8, U8ToU7};
