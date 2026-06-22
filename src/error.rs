//! Centralized error handling for deft.
//!
//! A single concrete error enum keeps things flat and pragmatic. Every
//! fallible operation in deft returns `Result<T, DeftError>`. We deliberately
//! avoid a trait-object based error hierarchy; a rich sum type is clearer and
//! lets call sites `match` on exactly what went wrong.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// The single error type used throughout deft.
#[derive(Debug)]
pub enum DeftError {
    /// An underlying I/O failure, annotated with the path it concerned.
    Io {
        path: Option<PathBuf>,
        source: io::Error,
    },

    /// The manifest (`deft.toml`) could not be parsed.
    ManifestParse { path: PathBuf, message: String },

    /// The lockfile (`deft.lock`) could not be parsed.
    LockParse { path: PathBuf, message: String },

    /// Serialization back to TOML failed.
    Serialize(String),

    /// A required file or directory in the strict layout was missing.
    LayoutViolation(String),

    /// The repository does not adhere to the deft standard.
    NotDeftStandard { path: PathBuf, reason: String },

    /// A dependency could not be resolved.
    Resolution(String),

    /// An external command (`git`, `curl`, `clang`) failed to even start.
    CommandSpawn { program: String, source: io::Error },

    /// An external command ran but exited non-zero.
    CommandFailed {
        program: String,
        code: Option<i32>,
        stderr: String,
    },

    /// Compilation failed; carries the count of failed translation units.
    Compilation { failures: usize },

    /// A configuration value was invalid (e.g. unknown optimization level).
    Config(String),

    /// The user's environment is missing something required (e.g. HOME).
    Environment(String),
}

impl fmt::Display for DeftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeftError::Io { path, source } => match path {
                Some(p) => write!(f, "I/O error at '{}': {}", p.display(), source),
                None => write!(f, "I/O error: {}", source),
            },
            DeftError::ManifestParse { path, message } => {
                write!(
                    f,
                    "failed to parse manifest '{}': {}",
                    path.display(),
                    message
                )
            }
            DeftError::LockParse { path, message } => {
                write!(
                    f,
                    "failed to parse lockfile '{}': {}",
                    path.display(),
                    message
                )
            }
            DeftError::Serialize(m) => write!(f, "failed to serialize: {}", m),
            DeftError::LayoutViolation(m) => write!(f, "project layout violation: {}", m),
            DeftError::NotDeftStandard { path, reason } => write!(
                f,
                "'{}' does not follow the deft standard: {}",
                path.display(),
                reason
            ),
            DeftError::Resolution(m) => write!(f, "dependency resolution failed: {}", m),
            DeftError::CommandSpawn { program, source } => {
                write!(
                    f,
                    "failed to launch '{}': {} (is it installed and on PATH?)",
                    program, source
                )
            }
            DeftError::CommandFailed {
                program,
                code,
                stderr,
            } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                write!(
                    f,
                    "'{}' exited with status {}:\n{}",
                    program,
                    code,
                    stderr.trim()
                )
            }
            DeftError::Compilation { failures } => {
                write!(
                    f,
                    "build failed: {} translation unit(s) did not compile",
                    failures
                )
            }
            DeftError::Config(m) => write!(f, "invalid configuration: {}", m),
            DeftError::Environment(m) => write!(f, "environment error: {}", m),
        }
    }
}

impl std::error::Error for DeftError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DeftError::Io { source, .. } => Some(source),
            DeftError::CommandSpawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for DeftError {
    fn from(source: io::Error) -> Self {
        DeftError::Io { path: None, source }
    }
}

/// Convenience alias so signatures stay short.
pub type Result<T> = std::result::Result<T, DeftError>;

/// Helper to attach a path to an io error after the fact.
pub trait IoPathExt<T> {
    fn path_ctx<P: Into<PathBuf>>(self, path: P) -> Result<T>;
}

impl<T> IoPathExt<T> for std::result::Result<T, io::Error> {
    fn path_ctx<P: Into<PathBuf>>(self, path: P) -> Result<T> {
        self.map_err(|source| DeftError::Io {
            path: Some(path.into()),
            source,
        })
    }
}
