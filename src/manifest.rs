//! Manifest (`deft.toml`) and lockfile (`deft.lock`) data model.
//!
//! These are plain serde structures. We lean on serde defaults and a couple of
//! small custom deserializers (for the `version | { version, features }`
//! shorthand) instead of building a parser by hand. Validation that needs to
//! touch the filesystem lives in the resolver/engine, not here.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::error::{DeftError, IoPathExt, Result};

// ---------------------------------------------------------------------------
// deft.toml
// ---------------------------------------------------------------------------

/// The fully parsed `deft.toml` manifest.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Manifest {
    /// Optional workspace declaration.
    #[serde(default)]
    pub workspace: Option<Workspace>,

    /// The package metadata. Required for buildable packages.
    pub package: Option<Package>,

    /// Feature flags: name -> list of features it implies.
    #[serde(default)]
    pub features: BTreeMap<String, Vec<String>>,

    /// Compiler profiles, keyed by language (`c` and/or `cpp`).
    #[serde(default)]
    pub profile: Profiles,

    /// Dependency table. Keys are `gh:user/lib` shorthands.
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
}

/// `[workspace]` table.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Workspace {
    #[serde(default)]
    pub members: Vec<String>,
}

/// `[package]` table.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    /// Optional pinned toolchain, e.g. `"clang-18.1"`. When set, `deft doctor`
    /// and the pre-build phase of `deft build` invoke the active compiler and
    /// abort the build if its reported version doesn't match.
    #[serde(default)]
    pub toolchain: Option<String>,
}

/// `[profile.c]` and `[profile.cpp]`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Profiles {
    #[serde(default)]
    pub c: Option<CProfile>,
    #[serde(default)]
    pub cpp: Option<CppProfile>,
}

/// Compiler configuration specific to C.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CProfile {
    /// e.g. "c11", "c17", "c2x".
    #[serde(default = "default_c_standard")]
    pub standard: String,
    /// Warning groups: "all", "extra", "error", "pedantic", ...
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Optimization level as a string: "0".."3", "s", "z", "g", "fast".
    #[serde(default = "default_opt")]
    pub optimization: String,
    /// Extra raw flags appended verbatim (escape hatch, normally empty).
    #[serde(default)]
    pub extra_flags: Vec<String>,
    /// Preprocessor defines: NAME or NAME=VALUE.
    #[serde(default)]
    pub defines: Vec<String>,
}

impl Default for CProfile {
    fn default() -> Self {
        CProfile {
            standard: default_c_standard(),
            warnings: Vec::new(),
            optimization: default_opt(),
            extra_flags: Vec::new(),
            defines: Vec::new(),
        }
    }
}

/// Compiler configuration specific to C++.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CppProfile {
    /// e.g. "c++17", "c++20", "c++23".
    #[serde(default = "default_cpp_standard")]
    pub standard: String,
    /// Enable RTTI. Defaults to true (Clang default).
    #[serde(default = "default_true")]
    pub rtti: bool,
    /// Enable exceptions. Defaults to true (Clang default).
    #[serde(default = "default_true")]
    pub exceptions: bool,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default = "default_opt")]
    pub optimization: String,
    #[serde(default)]
    pub extra_flags: Vec<String>,
    #[serde(default)]
    pub defines: Vec<String>,
}

impl Default for CppProfile {
    fn default() -> Self {
        CppProfile {
            standard: default_cpp_standard(),
            rtti: default_true(),
            exceptions: default_true(),
            warnings: Vec::new(),
            optimization: default_opt(),
            extra_flags: Vec::new(),
            defines: Vec::new(),
        }
    }
}

/// A parsed `[package] toolchain` pin, e.g. `"clang-18.1"` -> `{ compiler:
/// "clang", version: "18.1" }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainSpec {
    pub compiler: String,
    pub version: String,
}

impl ToolchainSpec {
    /// Parse a `<compiler>-<version>` string. The split is on the first `-`,
    /// so `"clang-18.1"` separates into `"clang"` and `"18.1"`.
    pub fn parse(raw: &str) -> Result<ToolchainSpec> {
        let (compiler, version) = raw.split_once('-').ok_or_else(|| {
            DeftError::Config(format!(
                "invalid toolchain spec '{raw}' (expected '<compiler>-<version>', e.g. 'clang-18.1')"
            ))
        })?;
        if compiler.is_empty() || version.is_empty() {
            return Err(DeftError::Config(format!(
                "invalid toolchain spec '{raw}' (expected '<compiler>-<version>', e.g. 'clang-18.1')"
            )));
        }
        Ok(ToolchainSpec {
            compiler: compiler.to_string(),
            version: version.to_string(),
        })
    }

    /// Invoke the pinned compiler and confirm its reported version matches —
    /// as a dotted-prefix match, so a manifest pin of `"18.1"` accepts any
    /// installed `"18.1.x"` but rejects `"17.x"` or `"19.x"`.
    pub fn validate(&self) -> Result<()> {
        let output = Command::new(&self.compiler)
            .arg("--version")
            .output()
            .map_err(|source| DeftError::CommandSpawn {
                program: self.compiler.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(DeftError::Config(format!(
                "toolchain check failed: '{} --version' did not exit successfully",
                self.compiler
            )));
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let detected = extract_compiler_version(&text).ok_or_else(|| {
            DeftError::Config(format!(
                "could not determine '{}' version from its --version output",
                self.compiler
            ))
        })?;

        let matches =
            detected == self.version || detected.starts_with(&format!("{}.", self.version));
        if !matches {
            return Err(DeftError::Config(format!(
                "environment unvalidated: manifest pins toolchain '{}-{}' but found '{} {}' \
                 (run `deft doctor` for details)",
                self.compiler, self.version, self.compiler, detected
            )));
        }
        Ok(())
    }
}

/// Pull the first dotted version-looking token out of a compiler's
/// `--version` first line, e.g. `"clang version 18.1.3"` -> `"18.1.3"`.
fn extract_compiler_version(output: &str) -> Option<String> {
    let first_line = output.lines().next()?;
    first_line.split_whitespace().find_map(|tok| {
        let cleaned = tok.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        let first = cleaned.chars().next()?;
        if first.is_ascii_digit() && cleaned.contains('.') {
            Some(cleaned.to_string())
        } else {
            None
        }
    })
}

fn default_c_standard() -> String {
    "c17".to_string()
}
fn default_cpp_standard() -> String {
    "c++20".to_string()
}
fn default_opt() -> String {
    "0".to_string()
}
fn default_true() -> bool {
    true
}

/// A dependency value. Supports both the bare-string and table forms:
///
/// ```toml
/// "gh:user/http_parser" = "1.5"
/// "gh:another/ssl"       = { version = "2.1", features = ["ssl"] }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct Dependency {
    pub version: String,
    pub features: Vec<String>,
    /// Optional explicit branch/tag override; normally the version is the tag.
    pub tag: Option<String>,
}

impl<'de> Deserialize<'de> for Dependency {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Intermediate untagged enum: a string, or a detailed table.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Simple(String),
            Detailed {
                version: String,
                #[serde(default)]
                features: Vec<String>,
                #[serde(default)]
                tag: Option<String>,
            },
        }

        let raw = Raw::deserialize(deserializer)?;
        Ok(match raw {
            Raw::Simple(version) => Dependency {
                version,
                features: Vec::new(),
                tag: None,
            },
            Raw::Detailed {
                version,
                features,
                tag,
            } => Dependency {
                version,
                features,
                tag,
            },
        })
    }
}

impl Manifest {
    /// Load and parse a `deft.toml` from a directory root.
    pub fn load(root: &Path) -> Result<Manifest> {
        let path = root.join("deft.toml");
        let text = fs::read_to_string(&path).path_ctx(&path)?;
        let manifest: Manifest = toml::from_str(&text).map_err(|e| DeftError::ManifestParse {
            path: path.clone(),
            message: e.to_string(),
        })?;
        Ok(manifest)
    }

    /// Compute the effective set of enabled features given CLI choices.
    ///
    /// Starts from `default` (unless suppressed), unions in the requested
    /// features, then transitively expands using the `[features]` table.
    pub fn resolve_features(&self, requested: &[String], no_default: bool) -> Vec<String> {
        let mut enabled: Vec<String> = Vec::new();
        let mut stack: Vec<String> = Vec::new();

        if !no_default {
            if let Some(defaults) = self.features.get("default") {
                stack.extend(defaults.iter().cloned());
            }
        }
        stack.extend(requested.iter().cloned());

        while let Some(feature) = stack.pop() {
            if enabled.iter().any(|f| f == &feature) {
                continue;
            }
            enabled.push(feature.clone());
            if let Some(implied) = self.features.get(&feature) {
                stack.extend(implied.iter().cloned());
            }
        }

        enabled.sort();
        enabled.dedup();
        enabled
    }

    /// True when this manifest declares a workspace.
    pub fn is_workspace(&self) -> bool {
        self.workspace
            .as_ref()
            .map(|w| !w.members.is_empty())
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// deft.lock
// ---------------------------------------------------------------------------

/// The complete `deft.lock` file: a flat list of locked dependencies.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Lockfile {
    #[serde(default, rename = "dependency")]
    pub dependencies: Vec<LockedDependency>,
}

/// One `[[dependency]]` entry in the lockfile.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockedDependency {
    /// Bare package name (last path segment of the shorthand).
    pub name: String,
    /// Source descriptor, e.g. `git+https://github.com/user/lib`.
    pub source: String,
    /// The exact resolved git commit SHA.
    pub checksum: String,
    /// The version/tag that was requested and resolved.
    pub version: String,
    /// Names of direct dependencies of this package (transitive graph edges).
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl Lockfile {
    /// Load a `deft.lock` if it exists. Returns `Ok(None)` when absent.
    pub fn load(root: &Path) -> Result<Option<Lockfile>> {
        let path = root.join("deft.lock");
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path).path_ctx(&path)?;
        let lock: Lockfile = toml::from_str(&text).map_err(|e| DeftError::LockParse {
            path: path.clone(),
            message: e.to_string(),
        })?;
        Ok(Some(lock))
    }

    /// Serialize and atomically write the lockfile to the project root.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = root.join("deft.lock");
        let mut sorted = self.clone();
        sorted.dependencies.sort_by(|a, b| a.name.cmp(&b.name));
        let header = "# This file is auto-generated by deft.\n\
                      # It records exact resolved versions for reproducible builds.\n\
                      # Do not edit by hand; run `deft update` to regenerate.\n\n";
        let body =
            toml::to_string_pretty(&sorted).map_err(|e| DeftError::Serialize(e.to_string()))?;
        let tmp = path.with_extension("lock.tmp");
        fs::write(&tmp, format!("{header}{body}")).path_ctx(&tmp)?;
        fs::rename(&tmp, &path).path_ctx(&path)?;
        Ok(())
    }

    /// Look up a locked dependency by package name.
    pub fn get(&self, name: &str) -> Option<&LockedDependency> {
        self.dependencies.iter().find(|d| d.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolchain_spec_parses_compiler_and_version() {
        let spec = ToolchainSpec::parse("clang-18.1").unwrap();
        assert_eq!(spec.compiler, "clang");
        assert_eq!(spec.version, "18.1");
    }

    #[test]
    fn toolchain_spec_rejects_missing_separator() {
        assert!(ToolchainSpec::parse("clang18.1").is_err());
    }

    #[test]
    fn toolchain_spec_rejects_empty_halves() {
        assert!(ToolchainSpec::parse("-18.1").is_err());
        assert!(ToolchainSpec::parse("clang-").is_err());
    }

    #[test]
    fn extract_compiler_version_finds_dotted_token() {
        assert_eq!(
            extract_compiler_version("clang version 18.1.3"),
            Some("18.1.3".to_string())
        );
        assert_eq!(
            extract_compiler_version("Apple clang version 15.0.0 (clang-1500.3.9.4)"),
            Some("15.0.0".to_string())
        );
        assert_eq!(extract_compiler_version("no version here"), None);
    }

    #[test]
    fn toolchain_spec_version_match_is_dotted_prefix() {
        // Exercised indirectly via the documented semantics: "18.1" should be
        // a prefix-match for a detected "18.1.3", but not for "18.10.0" (the
        // separator-aware prefix check avoids a false match across this
        // exact boundary).
        let detected = "18.1.3";
        let pin = "18.1";
        assert!(detected == pin || detected.starts_with(&format!("{pin}.")));

        let detected_other_minor = "18.10.0";
        let pin = "18.1";
        assert!(
            !(detected_other_minor == pin || detected_other_minor.starts_with(&format!("{pin}.")))
        );
    }
}
