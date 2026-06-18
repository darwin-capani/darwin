//! Crate error types.
//!
//! Two layers:
//!   - [`SexprError`] — the small, total error set the generic S-expression
//!     lexer/parser ([`crate::sexpr`]) can produce. Kept separate so the
//!     panic-freedom coverage over `sexpr::parse` (the cargo-fuzz target and the
//!     in-tree randomized tests) enumerates exactly these, and the KiCad parser
//!     can wrap them with file context.
//!   - [`CanvasError`] — the crate-wide error every other module returns. It
//!     folds in `SexprError`, KiCad-parse failures, IPC/protocol faults, ERC
//!     setup faults, and I/O.
//!
//! `thiserror` gives each variant a `Display` string and `#[from]` conversions so
//! `?` flows naturally. `anyhow` is still used at the binary's top level
//! (`main.rs`) for ad-hoc context; library code returns the typed [`CanvasError`]
//! so callers can match.
//!
//! This module is the CONTRACT. Downstream agents return [`CanvasError`] /
//! `Result<T>` and may match its variants; they must NOT change the type.

use std::path::PathBuf;

use thiserror::Error;

/// Errors from the generic S-expression layer ([`crate::sexpr`]). Total: the
/// lexer/parser can only ever produce these.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SexprError {
    /// The source contained no tokens.
    #[error("empty s-expression input")]
    Empty,

    /// A `"` opened a string that never closed (or a trailing backslash).
    #[error("unterminated string starting at byte {offset}")]
    UnterminatedString { offset: usize },

    /// A `(` opened a list that never closed before end of input.
    #[error("unterminated list at byte {offset}")]
    UnterminatedList { offset: usize },

    /// A `)` appeared with no matching `(`.
    #[error("unexpected ')' at byte {offset}")]
    UnexpectedClose { offset: usize },

    /// Tokens remained after the first complete top-level value (`parse` only).
    #[error("trailing tokens after top-level value at byte {offset}: {found}")]
    TrailingTokens { offset: usize, found: String },

    /// List nesting exceeded the depth guard (adversarial input).
    #[error("s-expression nested too deeply at byte {offset}")]
    TooDeep { offset: usize },
}

/// The crate-wide error type. Every library function that can fail returns
/// `Result<T, CanvasError>` (aliased [`Result`]).
#[derive(Debug, Error)]
pub enum CanvasError {
    /// Filesystem / I/O failure (reading a project file, writing the cache).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The generic S-expression layer failed.
    #[error("s-expression syntax: {0}")]
    Sexpr(#[from] SexprError),

    /// The S-expression was well-formed but not a valid KiCad document of the
    /// expected kind (missing root head, wrong version, malformed node). Carries
    /// the offending file for diagnostics.
    #[error("KiCad parse error in {path:?}: {message}")]
    Parse { path: PathBuf, message: String },

    /// A requested project path was outside the sandbox-permitted `projects/`
    /// (or `libraries/`) root, or otherwise not a permitted file. The IPC layer
    /// rejects the op rather than touching the path.
    #[error("path not permitted (must be inside the app's projects/ or libraries/): {0:?}")]
    PathNotPermitted(PathBuf),

    /// A referenced file does not exist.
    #[error("file not found: {0:?}")]
    NotFound(PathBuf),

    /// An unknown / unsupported file extension was opened (only `.kicad_sch`,
    /// `.kicad_pcb`, `.kicad_sym`, `.kicad_mod` are supported).
    #[error("unsupported file type {0:?} (expected a KiCad .kicad_sch/.kicad_pcb/.kicad_sym/.kicad_mod)")]
    UnsupportedFileType(PathBuf),

    /// A project file exceeded the in-app size cap before it was read. Bounds the
    /// heap/parse cost of one `project.open` in-process (defense in depth over the
    /// seatbelt/supervisor), and turns an adversarial multi-GiB file into a clean
    /// rejection instead of an OOM. Carries the path, the actual byte size, and the
    /// cap that was exceeded.
    #[error("file too large: {path:?} is {size} bytes, over the {cap}-byte cap")]
    FileTooLarge { path: PathBuf, size: u64, cap: u64 },

    /// An op referenced a net / component / layer that does not exist in the
    /// current scene.
    #[error("no such {kind}: {name:?}")]
    NoSuchEntity { kind: &'static str, name: String },

    /// An op arrived before a project was opened (e.g. `view.set` with no scene).
    #[error("no project is open")]
    NoProjectOpen,

    /// A trace op arrived in the wrong state (e.g. `trace.step` before
    /// `trace.start`, or `trace.start` with no net selected).
    #[error("invalid trace state: {0}")]
    TraceState(String),

    /// An inbound IPC line was not valid JSON / not a recognized op.
    #[error("malformed IPC message: {0}")]
    Protocol(String),

    /// JSON (de)serialization of an op / payload failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The capability token on an inbound line was missing or did not verify.
    /// (Token verification is the daemon's job per the runtime contract; this
    /// variant exists for the app's own defensive checks / self-test mode.)
    #[error("unauthorized: capability token missing or invalid")]
    Unauthorized,

    /// The render backend failed to initialize / a GPU surface error (only
    /// reachable with the `gpu` feature; device-gated).
    #[error("render error: {0}")]
    Render(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, CanvasError>;

impl CanvasError {
    /// Build a [`CanvasError::Parse`] with file context from any displayable
    /// message — the KiCad parser's common failure constructor.
    pub fn parse(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        CanvasError::Parse {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Build a [`CanvasError::Protocol`] from any displayable message.
    pub fn protocol(message: impl Into<String>) -> Self {
        CanvasError::Protocol(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sexpr_error_converts_into_canvas_error() {
        let e: CanvasError = SexprError::Empty.into();
        assert!(matches!(e, CanvasError::Sexpr(SexprError::Empty)));
        assert!(e.to_string().contains("empty s-expression"));
    }

    #[test]
    fn parse_constructor_carries_path() {
        let e = CanvasError::parse("/x/y.kicad_sch", "bad version");
        match e {
            CanvasError::Parse { path, message } => {
                assert_eq!(path, PathBuf::from("/x/y.kicad_sch"));
                assert_eq!(message, "bad version");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn io_error_converts() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let e: CanvasError = io.into();
        assert!(matches!(e, CanvasError::Io(_)));
    }
}
