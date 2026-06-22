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
mod manifest;
mod migrate;
mod resolver;

use std::path::{Path, PathBuf};
use std::process::Command;

use cli::{BuildArgs, Cli, Command as Cmd, InitArgs, RunArgs, UpdateArgs};
use compiler::Compiler;
use engine::{Crate, Engine, Layout, default_jobs, require_package};
use error::{DeftError, IoPathExt, Result};
use manifest::{Lockfile, Manifest};
use resolver::{ResolvedDep, Resolver, build_lockfile, package_name};

fn main() {
    let cli = Cli::parse_args();
    let verbose = cli.verbose > 0;
    let quiet = cli.quiet;

    let result = match cli.command {
        Cmd::Build(args) => cmd_build_top_level(args, verbose, quiet),
        Cmd::Run(args) => cmd_run(args, verbose, quiet),
        Cmd::Init(args) => cmd_init(args, quiet),
        Cmd::Update(args) => cmd_update(args, verbose, quiet),
        Cmd::Doctor => doctor::run(verbose),
        Cmd::Sync => cmd_sync(verbose, quiet),
        Cmd::Migrate(args) => migrate::run(&args, quiet),
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
}

/// Top-level `deft build` entry point.
fn cmd_build_top_level(args: BuildArgs, verbose: bool, quiet: bool) -> Result<()> {
    build_with_diagnostics(args, verbose, quiet).map(|_| ())
}

/// Run the (intentionally bare) build, and only pay for environment
/// diagnostics if it actually failed. A successful build never spawns a
/// single extra process beyond what compiling/linking already required —
/// that's what keeps the hot path at `deft build`'s target of a near-instant
/// invocation. Shared by `deft build` and `deft run`, since the latter is
/// just a build with an extra step.
fn build_with_diagnostics(args: BuildArgs, verbose: bool, quiet: bool) -> Result<BuildOutcome> {
    match cmd_build(args, verbose, quiet) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            if !quiet {
                eprintln!(
                    "\n\x1b[1;33mnote\x1b[0m: build failed — running `deft doctor` diagnostics...\n"
                );
                let _ = doctor::run(verbose);
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
fn cmd_build(args: BuildArgs, verbose: bool, quiet: bool) -> Result<BuildOutcome> {
    let root = project_root(args.manifest_path.as_deref())?;
    let manifest = Manifest::load(&root)?;

    if manifest.is_workspace() {
        // Workspaces build each member; we surface the last member's artifact.
        return build_workspace(&root, &manifest, &args, verbose, quiet);
    }

    build_single(&root, &manifest, &args, verbose, quiet)
}

/// Build a standalone (non-workspace) package.
fn build_single(
    root: &Path,
    manifest: &Manifest,
    args: &BuildArgs,
    verbose: bool,
    quiet: bool,
) -> Result<BuildOutcome> {
    let layout = Layout::assert_deft_standard(root)?;
    let package = require_package(manifest, root)?;

    // --- Dependency resolution -------------------------------------------
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

    // Build dependencies first so their archives/headers exist.
    let target_dir = root.join("target");
    let dep_includes = build_dependencies(&resolved, &resolver, args, verbose, quiet)?;

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

    let engine = Engine::new(jobs(args), verbose, quiet);
    let artifact = engine.build_package(
        &layout,
        &package,
        &compiler,
        &target_dir,
        args.output.as_deref(),
        args.release,
    )?;

    Ok(BuildOutcome {
        artifact,
        crate_kind: layout.crate_kind,
    })
}

/// Build every member of a workspace in declaration order.
fn build_workspace(
    root: &Path,
    manifest: &Manifest,
    args: &BuildArgs,
    verbose: bool,
    quiet: bool,
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
        let outcome = build_single(&member_root, &member_manifest, args, verbose, quiet)?;
        last = Some(outcome);
    }

    last.ok_or_else(|| DeftError::LayoutViolation("workspace has no members to build".into()))
}

/// Build all resolved dependencies and collect their include directories.
fn build_dependencies(
    resolved: &[ResolvedDep],
    _resolver: &Resolver,
    args: &BuildArgs,
    verbose: bool,
    quiet: bool,
) -> Result<Vec<PathBuf>> {
    let mut includes = Vec::new();

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
        let engine = Engine::new(jobs(args), verbose, quiet);

        // Force library output for dependencies even if they expose main.*.
        let lib_layout = Layout {
            crate_kind: Crate::Library,
            ..dep_layout.clone()
        };
        engine.build_package(
            &lib_layout,
            &dep_package,
            &dep_compiler,
            &dep_target,
            None,
            args.release,
        )?;

        // Expose the dependency's src/ as an include path (public headers).
        includes.push(dep.cache_path.join("src"));
        includes.push(dep.cache_path.join("include"));
    }

    Ok(includes)
}

/// `deft run`
fn cmd_run(args: RunArgs, verbose: bool, quiet: bool) -> Result<()> {
    let outcome = build_with_diagnostics(args.build, verbose, quiet)?;
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
    if sha.len() >= 10 { &sha[..10] } else { sha }
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
