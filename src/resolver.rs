//! Dependency resolution.
//!
//! Responsibilities:
//!   * Translate `gh:user/lib` shorthands into real repository URLs using the
//!     local mapping database `~/.deft/deft-libs`.
//!   * Clone (or reuse) dependencies in the global cache `~/.deft/cache/` at a
//!     specific tag, via the system `git` binary (with `curl` as a probe/
//!     fallback for reachability checks).
//!   * Pin the exact commit SHA and produce/consume `deft.lock`.
//!
//! The resolver shells out to real `git`/`curl` rather than embedding a VCS
//! library — this keeps the binary small and matches deft's design.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{DeftError, IoPathExt, Result};
use crate::manifest::{Dependency, LockedDependency, Lockfile, Manifest};

/// A single resolved dependency, ready to be built and recorded.
#[derive(Debug, Clone)]
pub struct ResolvedDep {
    /// Bare package name (last segment of the shorthand path).
    pub name: String,
    /// Original shorthand key, e.g. `gh:user/http_parser`. Retained for
    /// future tooling (e.g. richer `deft update` diagnostics) that needs to
    /// echo back exactly what the manifest declared.
    #[allow(dead_code)]
    pub shorthand: String,
    /// Concrete repository URL. Retained for future tooling (e.g. `deft
    /// doctor` connectivity checks) that needs the resolved origin.
    #[allow(dead_code)]
    pub url: String,
    /// `git+<url>` source descriptor for the lockfile.
    pub source: String,
    /// Requested version/tag.
    pub version: String,
    /// Resolved commit SHA.
    pub checksum: String,
    /// Absolute path to the checked-out copy inside the global cache.
    pub cache_path: PathBuf,
    /// Names of this dependency's own direct dependencies.
    pub dependencies: Vec<String>,
}

/// Owns the deft home directories and the shorthand mapping table.
pub struct Resolver {
    /// `~/.deft`
    home: PathBuf,
    /// `~/.deft/cache`
    cache: PathBuf,
    /// Parsed contents of `~/.deft/deft-libs`: shorthand -> url.
    mappings: BTreeMap<String, String>,
    verbose: bool,
}

impl Resolver {
    /// Build a resolver, discovering and (if needed) creating the deft home.
    pub fn new(verbose: bool) -> Result<Resolver> {
        let home = deft_home()?;
        let cache = home.join("cache");
        fs::create_dir_all(&cache).path_ctx(&cache)?;
        let mappings = load_mappings(&home)?;
        Ok(Resolver {
            home,
            cache,
            mappings,
            verbose,
        })
    }

    /// Path to the deft home directory (`~/.deft`). Retained for future
    /// tooling (e.g. `deft doctor`) that needs to report where deft's state
    /// lives without reaching into private fields.
    #[allow(dead_code)]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Resolve all dependencies of a manifest.
    ///
    /// If `lock` is `Some`, resolution is pinned to the recorded SHAs (the
    /// reproducible path used by `deft build`). If `None`, fresh resolution is
    /// performed and new SHAs are fetched (the `deft update` path).
    pub fn resolve_all(
        &self,
        manifest: &Manifest,
        lock: Option<&Lockfile>,
    ) -> Result<Vec<ResolvedDep>> {
        let mut resolved = Vec::new();
        for (shorthand, dep) in &manifest.dependencies {
            let locked = lock.and_then(|l| l.get(&package_name(shorthand)));
            let r = self.resolve_one(shorthand, dep, locked)?;
            resolved.push(r);
        }
        Ok(resolved)
    }

    /// Resolve a single dependency entry.
    fn resolve_one(
        &self,
        shorthand: &str,
        dep: &Dependency,
        locked: Option<&LockedDependency>,
    ) -> Result<ResolvedDep> {
        let name = package_name(shorthand);
        let url = self.map_shorthand(shorthand)?;
        let source = format!("git+{url}");
        let tag = dep.tag.clone().unwrap_or_else(|| dep.version.clone());

        let dest = self.cache.join(format!("{name}-{tag}"));

        // Ensure the repository is present in the cache at the requested tag.
        self.ensure_cached(&url, &tag, &dest)?;

        // Determine the SHA: trust the lock if present, else read from the
        // freshly checked-out tree.
        let checksum = match locked {
            Some(l) if l.version == dep.version => {
                // Reproducible path: hard-reset the cache to the locked SHA so
                // the on-disk tree matches the lockfile exactly.
                self.checkout_sha(&dest, &l.checksum)?;
                l.checksum.clone()
            }
            _ => self.head_sha(&dest)?,
        };

        // Read the dependency's own manifest to discover transitive edges.
        let sub_deps = self.direct_dependency_names(&dest)?;

        Ok(ResolvedDep {
            name,
            shorthand: shorthand.to_string(),
            url,
            source,
            version: dep.version.clone(),
            checksum,
            cache_path: dest,
            dependencies: sub_deps,
        })
    }

    /// Translate a `gh:user/lib` shorthand into a concrete URL.
    ///
    /// Resolution order:
    ///   1. Exact match in the `deft-libs` mapping file.
    ///   2. Built-in heuristic for the `gh:` prefix -> github.com.
    fn map_shorthand(&self, shorthand: &str) -> Result<String> {
        if let Some(url) = self.mappings.get(shorthand) {
            return Ok(url.clone());
        }
        if let Some(rest) = shorthand.strip_prefix("gh:") {
            if rest.split('/').count() == 2 && !rest.is_empty() {
                return Ok(format!("https://github.com/{rest}.git"));
            }
        }
        Err(DeftError::Resolution(format!(
            "no mapping for '{shorthand}' in {} and it is not a recognized shorthand",
            self.home.join("deft-libs").display()
        )))
    }

    /// Make sure `dest` contains a clone of `url` at `tag`.
    ///
    /// If `dest` already exists and looks like a git repo, we reuse it and just
    /// fetch + checkout the tag. Otherwise we do a fresh, shallow clone.
    fn ensure_cached(&self, url: &str, tag: &str, dest: &Path) -> Result<()> {
        if dest.join(".git").is_dir() {
            self.log(&format!("reusing cached {} @ {}", url, tag));
            // Fetch the specific tag in case the cache predates it.
            self.git(
                dest.parent().unwrap_or(&self.cache),
                &[
                    "-C",
                    &dest.to_string_lossy(),
                    "fetch",
                    "--depth",
                    "1",
                    "origin",
                    "tag",
                    tag,
                ],
            )
            .ok();
            self.checkout_tag(dest, tag)?;
            return Ok(());
        }

        // Probe reachability with curl before a potentially slow clone; this
        // produces a friendlier error for typo'd / private URLs.
        self.probe_url(url)?;

        self.log(&format!("cloning {} @ {} -> {}", url, tag, dest.display()));
        // Shallow clone directly at the tag for speed and determinism.
        let parent = dest.parent().unwrap_or(&self.cache);
        let status = self.git(
            parent,
            &[
                "clone",
                "--depth",
                "1",
                "--branch",
                tag,
                url,
                &dest.to_string_lossy(),
            ],
        );

        // Some tags are unannotated or the host disallows `--branch <tag>` on
        // shallow clone; fall back to a full clone + checkout.
        if status.is_err() {
            self.log("shallow tagged clone failed; retrying with full clone");
            if dest.exists() {
                fs::remove_dir_all(dest).path_ctx(dest)?;
            }
            self.git(parent, &["clone", url, &dest.to_string_lossy()])?;
            self.checkout_tag(dest, tag)?;
        }

        Ok(())
    }

    /// `git checkout <tag>` inside a repository.
    fn checkout_tag(&self, repo: &Path, tag: &str) -> Result<()> {
        self.git(
            repo,
            &["-C", &repo.to_string_lossy(), "checkout", "--quiet", tag],
        )?;
        Ok(())
    }

    /// Hard-reset a repository to an exact SHA (reproducible builds).
    fn checkout_sha(&self, repo: &Path, sha: &str) -> Result<()> {
        // Make sure the object exists locally; deepen if this was a shallow clone.
        if self
            .git(
                repo,
                &["-C", &repo.to_string_lossy(), "cat-file", "-e", sha],
            )
            .is_err()
        {
            self.git(
                repo,
                &["-C", &repo.to_string_lossy(), "fetch", "--unshallow"],
            )
            .ok();
        }
        self.git(
            repo,
            &["-C", &repo.to_string_lossy(), "checkout", "--quiet", sha],
        )?;
        Ok(())
    }

    /// Read the current HEAD commit SHA of a checked-out repository.
    fn head_sha(&self, repo: &Path) -> Result<String> {
        let output = run_capture("git", &["-C", &repo.to_string_lossy(), "rev-parse", "HEAD"])?;
        Ok(output.trim().to_string())
    }

    /// Inspect a cached dependency's own `deft.toml` for its direct deps.
    fn direct_dependency_names(&self, repo: &Path) -> Result<Vec<String>> {
        let manifest_path = repo.join("deft.toml");
        if !manifest_path.exists() {
            return Err(DeftError::NotDeftStandard {
                path: repo.to_path_buf(),
                reason: "missing deft.toml".to_string(),
            });
        }
        let sub = Manifest::load(repo)?;
        let mut names: Vec<String> = sub.dependencies.keys().map(|k| package_name(k)).collect();
        names.sort();
        Ok(names)
    }

    /// Use curl to verify a remote URL is reachable before cloning.
    fn probe_url(&self, url: &str) -> Result<()> {
        // Strip a trailing `.git` for the HTTP probe; git smart-HTTP serves
        // `<url>/info/refs?service=git-upload-pack`.
        let base = url.trim_end_matches(".git");
        let probe = format!("{base}/info/refs?service=git-upload-pack");
        let result = run_capture(
            "curl",
            &[
                "--silent",
                "--show-error",
                "--head",
                "--location",
                "--max-time",
                "20",
                "--fail",
                &probe,
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(_) => {
                // curl may be unavailable or the host blocks HEAD; don't hard
                // fail here — let the actual git clone be the source of truth.
                self.log(&format!("curl probe inconclusive for {url}; proceeding"));
                Ok(())
            }
        }
    }

    /// Run a git command rooted in `cwd`, mapping failures into DeftError.
    fn git(&self, cwd: &Path, args: &[&str]) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.current_dir(cwd).args(args);
        let output = cmd.output().map_err(|source| DeftError::CommandSpawn {
            program: "git".to_string(),
            source,
        })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(DeftError::CommandFailed {
                program: format!("git {}", args.join(" ")),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            })
        }
    }

    fn log(&self, msg: &str) {
        if self.verbose {
            eprintln!("  \x1b[2m[resolver]\x1b[0m {msg}");
        }
    }

    /// Refresh `~/.deft/deft-libs` from the registry's flat-text index.
    ///
    /// Deliberately shells out to whatever native fetch tool the OS already
    /// has — PowerShell's `Invoke-WebRequest` on Windows, `curl` (falling
    /// back to `wget`) elsewhere — instead of linking an HTTP client crate.
    /// That keeps the dependency tree and compile times exactly as small as
    /// the rest of deft.
    pub fn sync_index(&self, quiet: bool) -> Result<()> {
        let url =
            std::env::var("DEFT_LIBS_URL").unwrap_or_else(|_| DEFT_LIBS_INDEX_URL.to_string());
        let dest = self.home.join("deft-libs");
        let tmp = self.home.join("deft-libs.tmp");

        if !quiet {
            println!("\x1b[1;32m    Syncing\x1b[0m package index from {url}");
        }
        self.log(&format!("fetching {url} -> {}", tmp.display()));

        fetch_to_file(&url, &tmp)?;
        // Atomic overwrite: the rename is the only visible mutation of the
        // real index file, so a fetch that dies partway never corrupts it.
        fs::rename(&tmp, &dest).path_ctx(&dest)?;

        if !quiet {
            println!(
                "\x1b[1;32m    Updated\x1b[0m package index at {}",
                dest.display()
            );
        }
        Ok(())
    }
}

/// Default location of the flat-text package index. Overridable via
/// `DEFT_LIBS_URL` for self-hosted or air-gapped registries.
const DEFT_LIBS_INDEX_URL: &str =
    "https://raw.githubusercontent.com/deft-cli/deft-libs/main/deft-libs";

/// Fetch `url` into `dest` using only OS-native tools — no HTTP crate.
fn fetch_to_file(url: &str, dest: &Path) -> Result<()> {
    if cfg!(target_os = "windows") {
        return fetch_with_powershell(url, dest);
    }
    fetch_with_curl_or_wget(url, dest)
}

/// Windows path: native `Invoke-WebRequest` via PowerShell.
fn fetch_with_powershell(url: &str, dest: &Path) -> Result<()> {
    let ps = format!(
        "Invoke-WebRequest -Uri '{}' -OutFile '{}'",
        url,
        dest.display()
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", ps.as_str()])
        .status()
        .map_err(|source| DeftError::CommandSpawn {
            program: "powershell".to_string(),
            source,
        })?;
    if !status.success() {
        return Err(DeftError::CommandFailed {
            program: "powershell".to_string(),
            code: status.code(),
            stderr: format!("Invoke-WebRequest failed for {url}"),
        });
    }
    Ok(())
}

/// Linux/macOS path: `curl` with `wget` as a fail-safe fallback.
fn fetch_with_curl_or_wget(url: &str, dest: &Path) -> Result<()> {
    let dest_str = dest.to_string_lossy().to_string();

    let curl = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "--max-time",
            "30",
            "-o",
            dest_str.as_str(),
            url,
        ])
        .status();

    if let Ok(status) = curl {
        if status.success() {
            return Ok(());
        }
    }

    // curl missing or failed — fall back to wget.
    let status = Command::new("wget")
        .args(["--quiet", "--timeout=30", "-O", dest_str.as_str(), url])
        .status()
        .map_err(|source| DeftError::CommandSpawn {
            program: "curl/wget".to_string(),
            source,
        })?;
    if !status.success() {
        return Err(DeftError::CommandFailed {
            program: "wget".to_string(),
            code: status.code(),
            stderr: format!("failed to fetch {url}"),
        });
    }
    Ok(())
}

/// Build a fresh lockfile from a set of resolved dependencies.
pub fn build_lockfile(resolved: &[ResolvedDep]) -> Lockfile {
    let dependencies = resolved
        .iter()
        .map(|r| LockedDependency {
            name: r.name.clone(),
            source: r.source.clone(),
            checksum: r.checksum.clone(),
            version: r.version.clone(),
            dependencies: r.dependencies.clone(),
        })
        .collect();
    Lockfile { dependencies }
}

/// Extract the bare package name from a shorthand like `gh:user/http_parser`.
pub fn package_name(shorthand: &str) -> String {
    shorthand
        .rsplit('/')
        .next()
        .unwrap_or(shorthand)
        .trim_end_matches(".git")
        .to_string()
}

/// Resolve `~/.deft`, honoring `$DEFT_HOME` then `$HOME`.
fn deft_home() -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("DEFT_HOME") {
        if !explicit.is_empty() {
            return Ok(PathBuf::from(explicit));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| DeftError::Environment("HOME is not set; cannot locate ~/.deft".into()))?;
    if home.is_empty() {
        return Err(DeftError::Environment("HOME is empty".into()));
    }
    Ok(PathBuf::from(home).join(".deft"))
}

/// Parse `~/.deft/deft-libs`.
///
/// Format is line-oriented: `shorthand <whitespace> url`, with `#` comments
/// and blank lines ignored. Missing file is treated as an empty table so deft
/// still works with built-in `gh:` heuristics.
fn load_mappings(home: &Path) -> Result<BTreeMap<String, String>> {
    let path = home.join("deft-libs");
    let mut map = BTreeMap::new();
    if !path.exists() {
        return Ok(map);
    }
    let text = fs::read_to_string(&path).path_ctx(&path)?;
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let key = parts.next();
        let url = parts.next();
        match (key, url) {
            (Some(k), Some(u)) => {
                map.insert(k.to_string(), u.to_string());
            }
            _ => {
                return Err(DeftError::Resolution(format!(
                    "malformed mapping in {} on line {}: '{}'",
                    path.display(),
                    lineno + 1,
                    raw
                )));
            }
        }
    }
    Ok(map)
}

/// Run a command and capture stdout as a String, mapping errors to DeftError.
fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|source| DeftError::CommandSpawn {
            program: program.to_string(),
            source,
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(DeftError::CommandFailed {
            program: format!("{program} {}", args.join(" ")),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}
