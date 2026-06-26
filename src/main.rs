//! deft — entry point.
//!
//! `main` is a thin bridge: parse the CLI, dispatch to a handler, and turn a
//! `DeftError` into a clean, non-panicking process exit. All real logic lives
//! in the dedicated modules.

mod cli;
mod compiler;
mod doctor;
mod engine;
mod error;
mod hash;
mod json;
mod manifest;
mod migrate;
mod resolver;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use cli::{BuildArgs, Cli, Command as Cmd, InitArgs, RunArgs, UpdateArgs, VendorArgs};
use compiler::Compiler;
use engine::{default_jobs, require_package, Crate, Engine, Layout};
use error::{DeftError, IoPathExt, Result};
use json::Json;
use manifest::{Lockfile, Manifest, ToolchainSpec};
use resolver::{build_lockfile, package_name, ResolvedDep, Resolver};

fn main() {
    let cli = Cli::parse_args();
    let verbose = cli.verbose > 0;
    let quiet = cli.quiet;
    let json = cli.json;

    let result = match cli.command {
        Cmd::Build(args) => cmd_build_top_level(args, verbose, quiet, json),
        Cmd::Run(args) => cmd_run(args, verbose, quiet),
        Cmd::Init(args) => cmd_init(args, quiet),
        Cmd::Update(args) => cmd_update(args, verbose, quiet),
        Cmd::Doctor => doctor::run(verbose, json),
        Cmd::Sync => cmd_sync(verbose, quiet),
        Cmd::Migrate(args) => migrate::run(&args, quiet),
        Cmd::Vendor(args) => cmd_vendor(args, verbose, quiet),
    };

    if let Err(err) = result {
        eprintln!("\x1b[1;31merror\x1b[0m: {err}");
        // Print the cause chain for deeper context.
        let mut source = std::error::Error::source(&err);
        while let Some(cause) = source {
            eprintln!("  \x1b[2mcaused by:\x1b[0m {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}

/// Outcome of a successful build, reused by `run`.
struct BuildOutcome {
    artifact: PathBuf,
    crate_kind: Crate,
    /// How many library packages (this build's dependencies and/or the root
    /// package itself) were served from the global build cache instead of
    /// being recompiled.
    cache_hits: usize,
}

/// Top-level `deft build` entry point.
fn cmd_build_top_level(args: BuildArgs, verbose: bool, quiet: bool, json: bool) -> Result<()> {
    if !json {
        return build_with_diagnostics(args, verbose, quiet, json).map(|_| ());
    }

    let started = Instant::now();
    let result = build_with_diagnostics(args, verbose, true, true);
    let duration_ms = started.elapsed().as_millis() as i64;

    let payload = match &result {
        Ok(outcome) => build_success_payload(outcome, duration_ms),
        Err(err) => build_failure_payload(err, duration_ms),
    };
    println!("{}", payload.render());

    result.map(|_| ())
}

/// `{"status":"success","duration_ms":N,"cache_hits":N,"artifact":"...","errors":[]}`
fn build_success_payload(outcome: &BuildOutcome, duration_ms: i64) -> Json {
    Json::Object(vec![
        ("status".to_string(), Json::str("success")),
        ("duration_ms".to_string(), Json::Number(duration_ms)),
        (
            "cache_hits".to_string(),
            Json::Number(outcome.cache_hits as i64),
        ),
        (
            "artifact".to_string(),
            Json::str(outcome.artifact.display().to_string()),
        ),
        ("errors".to_string(), Json::Array(Vec::new())),
    ])
}

/// `{"status":"failure","duration_ms":N,"cache_hits":0,"errors":[{...}]}` —
/// `errors` carries the structured `CompileDiagnostic`s from
/// `DeftError::Compilation` when available, or a single synthetic entry
/// built from the error's `Display` text otherwise (e.g. a layout violation
/// that never reached the compiler at all).
fn build_failure_payload(err: &DeftError, duration_ms: i64) -> Json {
    let errors: Vec<Json> = match err {
        DeftError::Compilation { diagnostics, .. } => diagnostics
            .iter()
            .map(|d| {
                Json::Object(vec![
                    ("file".to_string(), Json::str(d.file.display().to_string())),
                    ("line".to_string(), Json::Number(d.line as i64)),
                    ("column".to_string(), Json::Number(d.column as i64)),
                    ("severity".to_string(), Json::str(d.severity)),
                    ("message".to_string(), Json::str(d.message.clone())),
                ])
            })
            .collect(),
        other => vec![Json::Object(vec![
            ("file".to_string(), Json::Null),
            ("line".to_string(), Json::Number(0)),
            ("column".to_string(), Json::Number(0)),
            ("severity".to_string(), Json::str("error")),
            ("message".to_string(), Json::str(other.to_string())),
        ])],
    };

    Json::Object(vec![
        ("status".to_string(), Json::str("failure")),
        ("duration_ms".to_string(), Json::Number(duration_ms)),
        ("cache_hits".to_string(), Json::Number(0)),
        ("errors".to_string(), Json::Array(errors)),
    ])
}

/// Run the (intentionally bare) build, and only pay for environment
/// diagnostics if it actually failed. A successful build never spawns a
/// single extra process beyond what compiling/linking already required —
/// that's what keeps the hot path at `deft build`'s target of a near-instant
/// invocation. Shared by `deft build` and `deft run`, since the latter is
/// just a build with an extra step.
fn build_with_diagnostics(
    args: BuildArgs,
    verbose: bool,
    quiet: bool,
    json: bool,
) -> Result<BuildOutcome> {
    match cmd_build(args, verbose, quiet, json) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            if !quiet && !json {
                eprintln!(
                    "\n\x1b[1;33mnote\x1b[0m: build failed — running `deft doctor` diagnostics...\n"
                );
                let _ = doctor::run(verbose, false);
                eprintln!();
            }
            Err(err)
        }
    }
}

/// `deft sync` — refresh `~/.deft/deft-libs` from the registry.
///
/// Strictly an index refresh: no manifest is loaded, no dependency is
/// resolved, and `deft.lock` is never touched. Use `deft update` to
/// re-resolve a project's dependencies instead.
fn cmd_sync(verbose: bool, quiet: bool) -> Result<()> {
    let resolver = Resolver::new(verbose)?;
    resolver.sync_index(quiet)
}

/// `deft build`
fn cmd_build(args: BuildArgs, verbose: bool, quiet: bool, json: bool) -> Result<BuildOutcome> {
    let root = project_root(args.manifest_path.as_deref())?;
    let manifest = Manifest::load(&root)?;

    if manifest.is_workspace() {
        // Workspaces build each member; we surface the last member's artifact.
        return build_workspace(&root, &manifest, &args, verbose, quiet, json);
    }

    build_single(&root, &manifest, &args, verbose, quiet, json)
}

/// Build a standalone (non-workspace) package.
fn build_single(
    root: &Path,
    manifest: &Manifest,
    args: &BuildArgs,
    verbose: bool,
    quiet: bool,
    json: bool,
) -> Result<BuildOutcome> {
    let layout = Layout::assert_deft_standard(root)?;
    let package = require_package(manifest, root)?;

    // --- Toolchain pin (opt-in; skipped entirely when unset, preserving the
    // hot-path guarantee documented in architecture.md) -------------------
    if let Some(spec) = &package.toolchain {
        ToolchainSpec::parse(spec)?.validate()?;
    }

    // --- Dependency resolution -------------------------------------------
    // A populated third_party/ takes over entirely: no git, no network, no
    // global resolver cache (see `deft vendor`).
    let resolved = match vendored_dependencies(root, manifest)? {
        Some(vendored) => vendored,
        None => {
            let resolver = Resolver::new(verbose)?;
            let existing_lock = Lockfile::load(root)?;
            let resolved = resolver.resolve_all(manifest, existing_lock.as_ref())?;

            // Write the lock if it was absent (first successful resolution).
            if existing_lock.is_none() && !resolved.is_empty() {
                let lock = build_lockfile(&resolved);
                lock.save(root)?;
                if !quiet {
                    println!(
                        "\x1b[1;32m    Locking\x1b[0m {} dependenc{}",
                        resolved.len(),
                        if resolved.len() == 1 { "y" } else { "ies" }
                    );
                }
            }
            resolved
        }
    };

    // Build dependencies first so their archives/headers exist.
    let target_dir = root.join("target");
    let (dep_includes, dep_cache_hits) = build_dependencies(&resolved, args, verbose, quiet, json)?;

    // --- Compile the root package ----------------------------------------
    let features = manifest.resolve_features(&args.features, args.no_default_features);
    if verbose && !features.is_empty() {
        eprintln!(
            "  \x1b[2m[deft]\x1b[0m active features: {}",
            features.join(", ")
        );
    }

    let compiler = Compiler::new(
        manifest.profile.c.clone().unwrap_or_default(),
        manifest.profile.cpp.clone().unwrap_or_default(),
        dep_includes,
        &features,
        args.release,
    );

    let engine = Engine::new(jobs(args), verbose, quiet, json);
    let built = engine.build_package(
        &layout,
        &package,
        &compiler,
        &target_dir,
        args.output.as_deref(),
        args.release,
    )?;

    Ok(BuildOutcome {
        artifact: built.path,
        crate_kind: layout.crate_kind,
        cache_hits: dep_cache_hits + if built.cache_hit { 1 } else { 0 },
    })
}

/// If `<root>/third_party` exists and is non-empty, resolve dependencies
/// entirely from local vendored copies plus `deft.lock` metadata, with no
/// git or network access at all — the offline/autonomous build path enabled
/// by `deft vendor`.
fn vendored_dependencies(root: &Path, manifest: &Manifest) -> Result<Option<Vec<ResolvedDep>>> {
    let vendor_dir = root.join("third_party");
    let has_entries = std::fs::read_dir(&vendor_dir)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if !has_entries {
        return Ok(None);
    }

    let lock = Lockfile::load(root)?.ok_or_else(|| {
        DeftError::Config(
            "third_party/ is populated but deft.lock is missing; run `deft vendor` again".into(),
        )
    })?;

    let mut resolved = Vec::with_capacity(manifest.dependencies.len());
    for shorthand in manifest.dependencies.keys() {
        let name = package_name(shorthand);
        let locked = lock.get(&name).ok_or_else(|| {
            DeftError::Config(format!(
                "vendored dependency '{name}' has no entry in deft.lock"
            ))
        })?;
        let cache_path = vendor_dir.join(&name);
        if !cache_path.is_dir() {
            return Err(DeftError::Config(format!(
                "third_party/{name} is missing; run `deft vendor` again"
            )));
        }
        resolved.push(ResolvedDep {
            name: name.clone(),
            shorthand: shorthand.clone(),
            url: locked.source.trim_start_matches("git+").to_string(),
            source: locked.source.clone(),
            version: locked.version.clone(),
            checksum: locked.checksum.clone(),
            cache_path,
            dependencies: locked.dependencies.clone(),
        });
    }
    Ok(Some(resolved))
}

/// Build every member of a workspace in declaration order.
fn build_workspace(
    root: &Path,
    manifest: &Manifest,
    args: &BuildArgs,
    verbose: bool,
    quiet: bool,
    json: bool,
) -> Result<BuildOutcome> {
    let members = manifest
        .workspace
        .as_ref()
        .map(|w| w.members.clone())
        .unwrap_or_default();

    let mut last: Option<BuildOutcome> = None;
    for member in &members {
        let member_root = root.join(member);
        let member_manifest = Manifest::load(&member_root)?;
        if !quiet {
            println!("\x1b[1;36m   Workspace\x1b[0m building member '{member}'");
        }
        let outcome = build_single(&member_root, &member_manifest, args, verbose, quiet, json)?;
        last = Some(outcome);
    }

    last.ok_or_else(|| DeftError::LayoutViolation("workspace has no members to build".into()))
}

/// Build all resolved dependencies and collect their include directories,
/// plus how many of them were served from the global build cache.
fn build_dependencies(
    resolved: &[ResolvedDep],
    args: &BuildArgs,
    verbose: bool,
    quiet: bool,
    json: bool,
) -> Result<(Vec<PathBuf>, usize)> {
    let mut includes = Vec::new();
    let mut cache_hits = 0usize;

    for dep in resolved {
        // Each dependency must itself be deft-standard.
        let dep_layout = Layout::assert_deft_standard(&dep.cache_path)?;
        let dep_manifest = Manifest::load(&dep.cache_path)?;
        let dep_package = require_package(&dep_manifest, &dep.cache_path)?;

        if !quiet {
            println!(
                "\x1b[1;32m   Compiling\x1b[0m {} v{} (dependency)",
                dep.name, dep.version
            );
        }

        // Dependencies are always built as libraries regardless of their own
        // entry kind hint — we link their archive into the consumer.
        let dep_features = dep_manifest.resolve_features(&[], false);
        let dep_compiler = Compiler::new(
            dep_manifest.profile.c.clone().unwrap_or_default(),
            dep_manifest.profile.cpp.clone().unwrap_or_default(),
            Vec::new(),
            &dep_features,
            args.release,
        );

        let dep_target = dep.cache_path.join("target");
        let engine = Engine::new(jobs(args), verbose, quiet, json);

        // Force library output for dependencies even if they expose main.*.
        let lib_layout = Layout {
            crate_kind: Crate::Library,
            ..dep_layout.clone()
        };
        let built = engine.build_package(
            &lib_layout,
            &dep_package,
            &dep_compiler,
            &dep_target,
            None,
            args.release,
        )?;
        if built.cache_hit {
            cache_hits += 1;
        }

        // Expose the dependency's src/ as an include path (public headers).
        includes.push(dep.cache_path.join("src"));
        includes.push(dep.cache_path.join("include"));
    }

    Ok((includes, cache_hits))
}

/// `deft run`
fn cmd_run(args: RunArgs, verbose: bool, quiet: bool) -> Result<()> {
    let outcome = build_with_diagnostics(args.build, verbose, quiet, false)?;
    if outcome.crate_kind != Crate::Executable {
        return Err(DeftError::LayoutViolation(
            "`deft run` requires an executable (src/main.cpp or src/main.c)".into(),
        ));
    }

    if !quiet {
        println!(
            "\x1b[1;32m     Running\x1b[0m {}",
            outcome.artifact.display()
        );
    }

    let status = Command::new(&outcome.artifact)
        .args(&args.bin_args)
        .status()
        .map_err(|source| DeftError::CommandSpawn {
            program: outcome.artifact.display().to_string(),
            source,
        })?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// `deft update` — re-resolve from scratch and rewrite the lockfile.
///
/// Strictly a dependency-graph refresh: it never fetches or rewrites the
/// package index (`~/.deft/deft-libs`). Use `deft sync` for that.
fn cmd_update(args: UpdateArgs, verbose: bool, quiet: bool) -> Result<()> {
    let root = project_root(args.manifest_path.as_deref())?;
    let manifest = Manifest::load(&root)?;
    let resolver = Resolver::new(verbose)?;

    // Pass `None` so resolution fetches fresh HEAD SHAs rather than honoring
    // the existing lock. If a single package was named, keep the others pinned.
    let existing = Lockfile::load(&root)?;
    let pin = match &args.package {
        Some(_) => existing.as_ref(),
        None => None,
    };

    let mut resolved = resolver.resolve_all(&manifest, pin)?;

    // If a specific package was requested, re-resolve only it freshly while the
    // rest stay at their locked SHAs (already applied above via `pin`).
    if let Some(only) = &args.package {
        let target = package_name(only);
        for dep in resolved.iter_mut() {
            if dep.name == target {
                // Force a fresh resolution by re-running without the pin.
                let fresh = resolver.resolve_all(&manifest, None)?;
                if let Some(updated) = fresh.into_iter().find(|d| d.name == target) {
                    *dep = updated;
                }
            }
        }
    }

    let lock = build_lockfile(&resolved);
    lock.save(&root)?;

    if !quiet {
        println!(
            "\x1b[1;32m     Updated\x1b[0m {} dependenc{} in deft.lock",
            resolved.len(),
            if resolved.len() == 1 { "y" } else { "ies" }
        );
        for dep in &resolved {
            println!(
                "            {} v{} @ {}",
                dep.name,
                dep.version,
                short_sha(&dep.checksum)
            );
        }
    }
    Ok(())
}

/// `deft vendor` — copy every dependency in `deft.lock` into a local
/// `third_party/` tree for complete offline autonomy.
///
/// Resolution reuses `Resolver::resolve_all` pinned to the existing lock (the
/// same reproducible path `deft build` takes), so vendoring never silently
/// re-resolves a dependency to a different commit than what's locked — it
/// only relocates already-resolved sources from the global cache into the
/// project itself.
fn cmd_vendor(args: VendorArgs, verbose: bool, quiet: bool) -> Result<()> {
    let root = project_root(args.manifest_path.as_deref())?;
    let manifest = Manifest::load(&root)?;
    let lock = Lockfile::load(&root)?.ok_or_else(|| {
        DeftError::Config("no deft.lock found; run `deft build` or `deft update` first".into())
    })?;

    let resolver = Resolver::new(verbose)?;
    let resolved = resolver.resolve_all(&manifest, Some(&lock))?;

    let vendor_dir = root.join("third_party");
    std::fs::create_dir_all(&vendor_dir).path_ctx(&vendor_dir)?;

    for dep in &resolved {
        let dest = vendor_dir.join(&dep.name);
        if dest.exists() {
            std::fs::remove_dir_all(&dest).path_ctx(&dest)?;
        }
        copy_tree_excluding_git(&dep.cache_path, &dest)?;
        if !quiet {
            println!(
                "\x1b[1;32m    Vendored\x1b[0m {} v{} -> {}",
                dep.name,
                dep.version,
                dest.display()
            );
        }
    }

    if !quiet {
        println!(
            "\x1b[1;32m   Finished\x1b[0m vendoring {} dependenc{} into {}",
            resolved.len(),
            if resolved.len() == 1 { "y" } else { "ies" },
            vendor_dir.display()
        );
    }
    Ok(())
}

/// Recursively copy a directory tree, skipping any `.git` directory — the
/// vendored copy is a source snapshot, not a git checkout.
fn copy_tree_excluding_git(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).path_ctx(dst)?;
    for entry in std::fs::read_dir(src).path_ctx(src)? {
        let entry = entry.path_ctx(src)?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let dest_path = dst.join(&name);
        if path.is_dir() {
            copy_tree_excluding_git(&path, &dest_path)?;
        } else {
            std::fs::copy(&path, &dest_path).path_ctx(&dest_path)?;
        }
    }
    Ok(())
}

/// `deft init` — scaffold a new deft-standard package.
fn cmd_init(args: InitArgs, quiet: bool) -> Result<()> {
    let root = &args.path;
    std::fs::create_dir_all(root).path_ctx(root)?;
    let src = root.join("src");
    std::fs::create_dir_all(&src).path_ctx(&src)?;

    let name = match &args.name {
        Some(n) => n.clone(),
        None => root
            .canonicalize()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| "my_project".to_string()),
    };

    let is_lib = args.lib && !args.bin;
    let is_c = args.c;

    // Pick entry file name + language.
    let (entry_file, entry_body) = match (is_lib, is_c) {
        (false, false) => ("main.cpp", CPP_MAIN),
        (false, true) => ("main.c", C_MAIN),
        (true, false) => ("lib.cpp", CPP_LIB),
        (true, true) => ("lib.c", C_LIB),
    };

    let entry_path = src.join(entry_file);
    if entry_path.exists() {
        return Err(DeftError::LayoutViolation(format!(
            "{} already exists; refusing to overwrite",
            entry_path.display()
        )));
    }
    std::fs::write(&entry_path, entry_body).path_ctx(&entry_path)?;

    // Write the manifest.
    let manifest_path = root.join("deft.toml");
    if !manifest_path.exists() {
        let profile = if is_c { C_PROFILE } else { CPP_PROFILE };
        let manifest = format!(
            "[package]\nname = \"{name}\"\nversion = \"0.2.0\"\n\n\
             [features]\ndefault = []\n\n{profile}\n[dependencies]\n"
        );
        std::fs::write(&manifest_path, manifest).path_ctx(&manifest_path)?;
    }

    // A minimal .gitignore so target/ doesn't get committed.
    let gitignore = root.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "/target\n").path_ctx(&gitignore)?;
    }

    if !quiet {
        let kind = if is_lib { "library" } else { "executable" };
        let lang = if is_c { "C" } else { "C++" };
        println!(
            "\x1b[1;32m     Created\x1b[0m {lang} {kind} package '{name}' at {}",
            root.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Resolve the project root from an optional `--manifest-path` (which may point
/// at a directory or a `deft.toml`) or fall back to the current directory.
fn project_root(explicit: Option<&Path>) -> Result<PathBuf> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let root = if path.is_file() {
        path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
    } else {
        path
    };
    if !root.join("deft.toml").is_file() {
        return Err(DeftError::LayoutViolation(format!(
            "no deft.toml found in {} (run `deft init` to create a package)",
            root.display()
        )));
    }
    Ok(root)
}

/// Effective job count for an invocation.
fn jobs(args: &BuildArgs) -> usize {
    args.jobs.unwrap_or_else(default_jobs).max(1)
}

fn short_sha(sha: &str) -> &str {
    if sha.len() >= 10 {
        &sha[..10]
    } else {
        sha
    }
}

// --- Scaffolding templates -------------------------------------------------

const CPP_MAIN: &str = "#include <iostream>\n\n\
int main() {\n    \
std::cout << \"Hello from deft!\" << std::endl;\n    \
return 0;\n}\n";

const C_MAIN: &str = "#include <stdio.h>\n\n\
int main(void) {\n    \
printf(\"Hello from deft!\\n\");\n    \
return 0;\n}\n";

const CPP_LIB: &str = "// Library entry point.\n\n\
int deft_add(int a, int b) {\n    \
return a + b;\n}\n";

const C_LIB: &str = "/* Library entry point. */\n\n\
int deft_add(int a, int b) {\n    \
return a + b;\n}\n";

const CPP_PROFILE: &str = "[profile.cpp]\nstandard = \"c++20\"\nrtti = false\n\
exceptions = true\nwarnings = [\"all\", \"extra\"]\noptimization = \"0\"\n";

const C_PROFILE: &str = "[profile.c]\nstandard = \"c17\"\n\
warnings = [\"all\", \"extra\"]\noptimization = \"0\"\n";

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::{Dependency, LockedDependency};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let n = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("deft-main-test-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manifest_with_dependency(shorthand: &str) -> Manifest {
        let mut manifest = Manifest::default();
        manifest.dependencies.insert(
            shorthand.to_string(),
            Dependency {
                version: "1.0".to_string(),
                features: Vec::new(),
                tag: None,
            },
        );
        manifest
    }

    #[test]
    fn vendored_dependencies_is_none_without_a_populated_third_party_dir() {
        let root = temp_dir("no-vendor");
        let manifest = manifest_with_dependency("gh:user/lib");
        assert!(vendored_dependencies(&root, &manifest).unwrap().is_none());

        // An empty third_party/ (created but with nothing in it) also counts
        // as "not vendored" — vendoring is only active once it has content.
        std::fs::create_dir_all(root.join("third_party")).unwrap();
        assert!(vendored_dependencies(&root, &manifest).unwrap().is_none());
    }

    #[test]
    fn vendored_dependencies_errors_when_lock_is_missing() {
        let root = temp_dir("missing-lock");
        std::fs::create_dir_all(root.join("third_party").join("lib")).unwrap();
        let manifest = manifest_with_dependency("gh:user/lib");

        let err = vendored_dependencies(&root, &manifest).unwrap_err();
        assert!(err.to_string().contains("deft.lock"));
    }

    #[test]
    fn vendored_dependencies_resolves_from_local_copies_and_lock_metadata() {
        let root = temp_dir("vendored");
        let dep_dir = root.join("third_party").join("lib");
        std::fs::create_dir_all(&dep_dir).unwrap();

        let lock = Lockfile {
            dependencies: vec![LockedDependency {
                name: "lib".to_string(),
                source: "git+https://example.com/user/lib.git".to_string(),
                checksum: "deadbeef".to_string(),
                version: "1.0".to_string(),
                dependencies: Vec::new(),
            }],
        };
        lock.save(&root).unwrap();

        let manifest = manifest_with_dependency("gh:user/lib");
        let resolved = vendored_dependencies(&root, &manifest)
            .unwrap()
            .expect("third_party/ is populated, so this must resolve locally");

        assert_eq!(resolved.len(), 1);
        let dep = &resolved[0];
        assert_eq!(dep.name, "lib");
        assert_eq!(dep.checksum, "deadbeef");
        assert_eq!(dep.cache_path, dep_dir);
        assert_eq!(dep.url, "https://example.com/user/lib.git");
    }

    #[test]
    fn vendored_dependencies_errors_when_a_dependency_directory_is_missing() {
        let root = temp_dir("partial-vendor");
        // third_party/ has *something* in it, but not the dependency itself.
        std::fs::create_dir_all(root.join("third_party").join("other")).unwrap();

        let lock = Lockfile {
            dependencies: vec![LockedDependency {
                name: "lib".to_string(),
                source: "git+https://example.com/user/lib.git".to_string(),
                checksum: "deadbeef".to_string(),
                version: "1.0".to_string(),
                dependencies: Vec::new(),
            }],
        };
        lock.save(&root).unwrap();

        let manifest = manifest_with_dependency("gh:user/lib");
        let err = vendored_dependencies(&root, &manifest).unwrap_err();
        assert!(err.to_string().contains("third_party/lib"));
    }

    #[test]
    fn copy_tree_excluding_git_skips_git_dir_and_copies_nested_files() {
        let root = temp_dir("copy-tree");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::create_dir_all(src.join(".git")).unwrap();
        std::fs::write(src.join("top.txt"), "top").unwrap();
        std::fs::write(src.join("nested").join("deep.txt"), "deep").unwrap();
        std::fs::write(src.join(".git").join("HEAD"), "ref: refs/heads/main").unwrap();

        let dst = root.join("dst");
        copy_tree_excluding_git(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("top.txt")).unwrap(), "top");
        assert_eq!(
            std::fs::read_to_string(dst.join("nested").join("deep.txt")).unwrap(),
            "deep"
        );
        assert!(!dst.join(".git").exists());
    }

    #[test]
    fn build_success_payload_renders_expected_fields() {
        let outcome = BuildOutcome {
            artifact: PathBuf::from("/tmp/target/debug/app"),
            crate_kind: Crate::Executable,
            cache_hits: 2,
        };
        let rendered = build_success_payload(&outcome, 1234).render();
        assert!(rendered.contains("\"status\":\"success\""));
        assert!(rendered.contains("\"duration_ms\":1234"));
        assert!(rendered.contains("\"cache_hits\":2"));
        assert!(rendered.contains("\"artifact\":\"/tmp/target/debug/app\""));
        assert!(rendered.contains("\"errors\":[]"));
    }

    #[test]
    fn build_failure_payload_surfaces_structured_compile_diagnostics() {
        let err = DeftError::Compilation {
            failures: 1,
            diagnostics: vec![error::CompileDiagnostic {
                file: PathBuf::from("src/main.c"),
                line: 4,
                column: 2,
                severity: "error",
                message: "undeclared identifier 'foo'".to_string(),
            }],
        };
        let rendered = build_failure_payload(&err, 42).render();
        assert!(rendered.contains("\"status\":\"failure\""));
        assert!(rendered.contains("\"duration_ms\":42"));
        assert!(rendered.contains("\"line\":4"));
        assert!(rendered.contains("undeclared identifier"));
    }

    #[test]
    fn build_failure_payload_falls_back_to_display_text_for_non_compilation_errors() {
        let err = DeftError::Config("bad toolchain spec".to_string());
        let rendered = build_failure_payload(&err, 7).render();
        assert!(rendered.contains("\"status\":\"failure\""));
        assert!(rendered.contains("bad toolchain spec"));
    }
}
