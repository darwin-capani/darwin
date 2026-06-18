//! Crate error type (FROZEN — written in Foundation, module agents do NOT
//! change it; they ADD variants only by appending, never reorder/remove).
//!
//! The realtime audio path NEVER returns these (it cannot allocate or unwind on
//! the audio thread — SPEC §2). These errors live on the CONTROL-PLANE-facing
//! edges: parameter validation, preset (de)serialization, FFI argument checks,
//! and the device-gated CoreAudio seam. The FFI layer maps each variant to a
//! small stable integer code ([`NexusError::code`]) that crosses the C ABI to
//! Python; the human string never crosses ctypes.

use std::fmt;

/// Stable C-ABI integer status codes returned by every fallible `extern "C"`
/// FFI entry point. `0` is success; negatives are errors. These integers are
/// part of the FFI CONTRACT — Python's ctypes layer switches on them — so they
/// are FROZEN: append new codes with new (more-negative) values, never renumber.
pub mod codes {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// A pointer argument was null where a valid pointer was required.
    pub const NULL_POINTER: i32 = -1;
    /// A length/count/index argument was out of the engine's configured bounds.
    pub const OUT_OF_BOUNDS: i32 = -2;
    /// A parameter value was non-finite (NaN/Inf) or outside its valid range.
    pub const INVALID_PARAM: i32 = -3;
    /// The opaque engine handle was null or did not belong to this crate.
    pub const INVALID_HANDLE: i32 = -4;
    /// A buffer's frame count / channel count did not match the engine config.
    pub const BUFFER_MISMATCH: i32 = -5;
    /// A preset (de)serialization or TOML I/O step failed.
    pub const PRESET_ERROR: i32 = -6;
    /// A CoreAudio / device-gated operation failed (NEVER reachable headlessly).
    pub const DEVICE_ERROR: i32 = -7;
    /// Catch-all for an internal invariant violation.
    pub const INTERNAL: i32 = -100;
}

/// The crate's error type. Every fallible control-plane / FFI-edge function
/// returns `Result<T, NexusError>`; the FFI shims convert it to a `codes::*`
/// integer via [`NexusError::code`]. The audio callback does NOT use this.
#[derive(Debug, Clone, PartialEq)]
pub enum NexusError {
    /// A required pointer argument was null.
    NullPointer {
        /// The name of the argument that was null (static, for diagnostics).
        arg: &'static str,
    },
    /// An index/count argument exceeded the engine's configured matrix bounds.
    OutOfBounds {
        /// What was out of bounds (e.g. "input index", "frame count").
        what: &'static str,
        /// The offending value.
        got: usize,
        /// The exclusive upper bound (or the exact required value).
        limit: usize,
    },
    /// A parameter was NaN/Inf or outside its documented valid range.
    InvalidParam {
        /// The parameter name (e.g. "gain_db", "threshold_db").
        param: &'static str,
        /// A short reason (e.g. "non-finite", "below -inf sentinel").
        reason: &'static str,
    },
    /// The opaque engine handle was null or not one this crate minted.
    InvalidHandle,
    /// A processing buffer did not match the engine's frame/channel config.
    BufferMismatch {
        /// What mismatched ("frames" or "channels").
        what: &'static str,
        /// The value the caller supplied.
        got: usize,
        /// The value the engine was configured for.
        expected: usize,
    },
    /// Preset TOML (de)serialization or file I/O failed.
    Preset(String),
    /// A CoreAudio / device-gated operation failed. NEVER produced on the
    /// headless path; only the `coreaudio`-feature code can return this.
    Device(String),
    /// An internal invariant was violated (a bug — should be unreachable).
    Internal(String),
}

impl NexusError {
    /// The stable C-ABI integer code this error maps to (see [`codes`]).
    pub fn code(&self) -> i32 {
        match self {
            NexusError::NullPointer { .. } => codes::NULL_POINTER,
            NexusError::OutOfBounds { .. } => codes::OUT_OF_BOUNDS,
            NexusError::InvalidParam { .. } => codes::INVALID_PARAM,
            NexusError::InvalidHandle => codes::INVALID_HANDLE,
            NexusError::BufferMismatch { .. } => codes::BUFFER_MISMATCH,
            NexusError::Preset(_) => codes::PRESET_ERROR,
            NexusError::Device(_) => codes::DEVICE_ERROR,
            NexusError::Internal(_) => codes::INTERNAL,
        }
    }
}

impl fmt::Display for NexusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NexusError::NullPointer { arg } => write!(f, "null pointer argument: {arg}"),
            NexusError::OutOfBounds { what, got, limit } => {
                write!(f, "{what} out of bounds: {got} (limit {limit})")
            }
            NexusError::InvalidParam { param, reason } => {
                write!(f, "invalid parameter {param}: {reason}")
            }
            NexusError::InvalidHandle => write!(f, "invalid or null engine handle"),
            NexusError::BufferMismatch { what, got, expected } => {
                write!(f, "buffer {what} mismatch: got {got}, expected {expected}")
            }
            NexusError::Preset(m) => write!(f, "preset error: {m}"),
            NexusError::Device(m) => write!(f, "device error: {m}"),
            NexusError::Internal(m) => write!(f, "internal error: {m}"),
        }
    }
}

impl std::error::Error for NexusError {}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, NexusError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_distinct() {
        // OK is 0; every error code is negative and unique. Renumbering these
        // would break the Python ctypes switch — this test pins them.
        assert_eq!(codes::OK, 0);
        let all = [
            codes::NULL_POINTER,
            codes::OUT_OF_BOUNDS,
            codes::INVALID_PARAM,
            codes::INVALID_HANDLE,
            codes::BUFFER_MISMATCH,
            codes::PRESET_ERROR,
            codes::DEVICE_ERROR,
            codes::INTERNAL,
        ];
        for c in all {
            assert!(c < 0, "error code {c} must be negative");
        }
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "error codes must be distinct");
    }

    #[test]
    fn error_maps_to_its_code() {
        assert_eq!(NexusError::InvalidHandle.code(), codes::INVALID_HANDLE);
        assert_eq!(
            NexusError::OutOfBounds { what: "input", got: 9, limit: 8 }.code(),
            codes::OUT_OF_BOUNDS
        );
        assert_eq!(NexusError::Preset("bad toml".into()).code(), codes::PRESET_ERROR);
    }
}
