//! Build engine: project layout enforcement, parallel compilation, linking,
//! and human-readable Clang diagnostics.
//!
//! Concurrency uses only the standard library: a pool of native threads pulls
//! `CompileUnit`s off a shared work queue (guarded by a `Mutex`) and reports
//! results back over an `mpsc` channel. No external job-server crate, no async
//! runtime — just `std::thread` + `std::sync`.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crate::compiler::{CompileUnit, Compiler, Language, LinkCommand};
use crate::error::{CompileDiagnostic, DeftError, IoPathExt, Result};
use crate::hash;
use crate::manifest::{Manifest, Package};
use crate::resolver;

/// What kind of artifact a package produces, decided by which entry file the
/// strict layout contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crate {
    Executable,
    Library,
}

/// The discovered, validated layout of a single package.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Kept for future diagnostics/migration tooling (e.g. `deft doctor`,
    /// `deft migrate`) that need the package root, not just `src/`.
    #[allow(dead_code)]
    pub root: PathBuf,
    pub src: PathBuf,
    /// Kept for future diagnostics that report the exact entry file found.
    #[allow(dead_code)]
    pub entry: PathBuf,
    pub entry_language: Language,
    pub crate_kind: Crate,
}

impl Layout {
    /// Enforce deft's strict layout. deft does NOT search for sources: the
    /// entry point must be exactly one of the four canonical files.
    ///
    /// Precedence: an executable entry (`main`) wins over a library entry if
    /// both somehow exist, because a package with `main.*` is runnable.
    pub fn discover(root: &Path) -> Result<Layout> {
        let src = root.join("src");
        if !src.is_dir() {
            return Err(DeftError::LayoutViolation(format!(
                "missing 'src/' directory under {}",
                root.display()
            )));
        }

        // Canonical entry candidates in priority order.
        let candidates: [(&str, Crate, Language); 4] = [
            ("main.cpp", Crate::Executable, Language::Cpp),
            ("main.c", Crate::Executable, Language::C),
            ("lib.cpp", Crate::Library, Language::Cpp),
            ("lib.c", Crate::Library, Language::C),
        ];

        for (file, kind, lang) in candidates {
            let entry = src.join(file);
            if entry.is_file() {
                return Ok(Layout {
                    root: root.to_path_buf(),
                    src,
                    entry,
                    entry_language: lang,
                    crate_kind: kind,
                });
            }
        }

        Err(DeftError::LayoutViolation(format!(
            "no entry point found: expected one of src/main.cpp, src/main.c, \
             src/lib.cpp, src/lib.c under {}",
            root.display()
        )))
    }

    /// Verify a directory is a valid deft-standard package (manifest + layout).
    pub fn assert_deft_standard(root: &Path) -> Result<Layout> {
        if !root.join("deft.toml").is_file() {
            return Err(DeftError::NotDeftStandard {
                path: root.to_path_buf(),
                reason: "missing deft.toml manifest".to_string(),
            });
        }
        Layout::discover(root).map_err(|e| DeftError::NotDeftStandard {
            path: root.to_path_buf(),
            reason: e.to_string(),
        })
    }

    /// Gather every compilable translation unit under `src/`, respecting the
    /// strict single-language rule: the entry language dictates which sources
    /// are eligible. Mixing C and C++ in one package is forbidden.
    pub fn collect_sources(&self) -> Result<Vec<PathBuf>> {
        let mut sources = Vec::new();
        let mut foreign = Vec::new();
        collect_sources_rec(&self.src, &mut sources, &mut foreign, self.entry_language)?;

        if !foreign.is_empty() {
            let other = match self.entry_language {
                Language::C => "C++",
                Language::Cpp => "C",
            };
            return Err(DeftError::LayoutViolation(format!(
                "strict C/C++ separation violated: this is a {} package but found \
                 {} {} source file(s) (e.g. '{}'). A deft package is single-language.",
                self.entry_language.label(),
                foreign.len(),
                other,
                foreign[0].display()
            )));
        }

        sources.sort();
        Ok(sources)
    }
}

/// Recursively collect sources matching `lang`; record mismatches in `foreign`.
fn collect_sources_rec(
    dir: &Path,
    matching: &mut Vec<PathBuf>,
    foreign: &mut Vec<PathBuf>,
    lang: Language,
) -> Result<()> {
    let entries = fs::read_dir(dir).path_ctx(dir)?;
    for entry in entries {
        let entry = entry.path_ctx(dir)?;
        let path = entry.path();
        if path.is_dir() {
            collect_sources_rec(&path, matching, foreign, lang)?;
        } else if let Some(found) = Language::from_extension(&path) {
            if found == lang {
                matching.push(path);
            } else {
                foreign.push(path);
            }
        }
    }
    Ok(())
}

/// Outcome of compiling one translation unit, sent back over the channel.
struct UnitResult {
    source: PathBuf,
    success: bool,
    diagnostics: Vec<Diagnostic>,
    raw_stderr: String,
}

/// The artifact produced by [`Engine::build_package`], plus whether it came
/// from the global build cache instead of a fresh compile.
pub struct BuiltArtifact {
    pub path: PathBuf,
    pub cache_hit: bool,
}

/// Top-level build orchestrator.
pub struct Engine {
    jobs: usize,
    verbose: bool,
    quiet: bool,
    /// When true, suppress human-readable progress/diagnostic text — the
    /// caller is rendering a single structured `--json` payload instead.
    json: bool,
}

impl Engine {
    pub fn new(jobs: usize, verbose: bool, quiet: bool, json: bool) -> Engine {
        let jobs = jobs.max(1);
        Engine {
            jobs,
            verbose,
            quiet,
            json,
        }
    }

    /// Compile and link a package. Returns the produced artifact's path and
    /// whether it was served from the global build cache.
    ///
    /// Only library artifacts (static archives) participate in the global
    /// cache — see docs/guides/architecture.md for why executables, whose
    /// output is project-specific, are out of scope for it.
    pub fn build_package(
        &self,
        layout: &Layout,
        package: &Package,
        compiler: &Compiler,
        target_dir: &Path,
        output_name: Option<&str>,
        release: bool,
    ) -> Result<BuiltArtifact> {
        compiler.validate()?;

        let profile_dir = target_dir.join(if release { "release" } else { "debug" });
        let obj_dir = profile_dir.join("obj").join(&package.name);
        fs::create_dir_all(&obj_dir).path_ctx(&obj_dir)?;

        let sources = layout.collect_sources()?;
        if sources.is_empty() {
            return Err(DeftError::LayoutViolation(format!(
                "package '{}' has no source files under src/",
                package.name
            )));
        }

        let artifact = artifact_path(&profile_dir, layout.crate_kind, &package.name, output_name);

        // --- Global cache short-circuit ---------------------------------
        // Before spinning up the compile thread-pool, see whether a
        // byte-identical build (same sources, same flags, same target) has
        // already been cached globally under ~/.deft/cache/prebuilt/{hash}.
        let cache_key = if layout.crate_kind == Crate::Library {
            let fingerprint = compiler.cache_fingerprint(layout.entry_language)?;
            Some(hash::package_key(&sources, &fingerprint)?)
        } else {
            None
        };

        if let Some(key) = &cache_key {
            if let Ok(home) = resolver::deft_home() {
                if let Some(cached) = hash::lookup(&home, key, &package.name) {
                    if let Some(parent) = artifact.parent() {
                        fs::create_dir_all(parent).path_ctx(parent)?;
                    }
                    fs::copy(&cached, &artifact).path_ctx(&artifact)?;
                    if !self.quiet {
                        println!(
                            "\x1b[1;32m  Cache hit\x1b[0m {} v{} [{}]",
                            package.name, package.version, key
                        );
                    }
                    return Ok(BuiltArtifact {
                        path: artifact,
                        cache_hit: true,
                    });
                }
            }
        }

        // Plan every translation unit.
        let mut units = Vec::with_capacity(sources.len());
        let mut has_cpp = false;
        for src in &sources {
            let obj = object_path(&obj_dir, layout, src);
            if let Some(parent) = obj.parent() {
                fs::create_dir_all(parent).path_ctx(parent)?;
            }
            let unit = compiler.compile_unit(src, &obj)?;
            if unit.language == Language::Cpp {
                has_cpp = true;
            }
            units.push(unit);
        }

        if !self.quiet {
            println!(
                "\x1b[1;32m   Compiling\x1b[0m {} v{} ({} unit{}, {} job{})",
                package.name,
                package.version,
                units.len(),
                plural(units.len()),
                self.jobs,
                plural(self.jobs),
            );
        }

        let started = Instant::now();
        let objects = self.compile_all(units)?;
        if self.verbose {
            eprintln!(
                "  \x1b[2m[engine]\x1b[0m compiled in {:.2}s",
                started.elapsed().as_secs_f64()
            );
        }

        if let Some(parent) = artifact.parent() {
            fs::create_dir_all(parent).path_ctx(parent)?;
        }

        let link = compiler.link_command(
            &objects,
            &artifact,
            has_cpp,
            layout.crate_kind == Crate::Library,
        );
        self.run_link(&link, layout.crate_kind)?;

        if !self.quiet {
            let kind = match layout.crate_kind {
                Crate::Executable => "executable",
                Crate::Library => "library",
            };
            println!(
                "\x1b[1;32m    Finished\x1b[0m {} {} [{}]",
                kind,
                artifact.display(),
                if release {
                    "optimized"
                } else {
                    "unoptimized + debuginfo"
                }
            );
        }

        // Populate the global cache for next time. Best-effort: a cache
        // write failure (e.g. an unwritable ~/.deft) must never fail an
        // otherwise-successful build.
        if let Some(key) = &cache_key {
            if let Ok(home) = resolver::deft_home() {
                let _ = hash::store(&home, key, &package.name, &artifact);
            }
        }

        Ok(BuiltArtifact {
            path: artifact,
            cache_hit: false,
        })
    }

    /// Run all compile units across a fixed-size thread pool.
    fn compile_all(&self, units: Vec<CompileUnit>) -> Result<Vec<PathBuf>> {
        let total = units.len();
        let objects: Vec<PathBuf> = units.iter().map(|u| u.object.clone()).collect();

        // Shared work queue + results channel.
        let queue: Arc<Mutex<VecDeque<CompileUnit>>> = Arc::new(Mutex::new(VecDeque::from(units)));
        let (tx, rx) = mpsc::channel::<UnitResult>();

        let worker_count = self.jobs.min(total).max(1);
        let mut handles = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let tx = tx.clone();
            let handle = thread::spawn(move || {
                loop {
                    // Pop one unit; release the lock before doing slow I/O.
                    let unit = {
                        let mut q = match queue.lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        q.pop_front()
                    };
                    let Some(unit) = unit else { break };

                    let result = run_compile(&unit);
                    // If the receiver hung up, just stop.
                    if tx.send(result).is_err() {
                        break;
                    }
                }
            });
            handles.push(handle);
        }
        // Drop our own sender so the channel closes once workers finish.
        drop(tx);

        // Collect results as they arrive.
        let mut failures = 0usize;
        let mut completed = 0usize;
        let mut failed_diagnostics: Vec<CompileDiagnostic> = Vec::new();
        for result in rx {
            completed += 1;
            self.report_unit(&result, completed, total);
            if !result.success {
                failures += 1;
                collect_failure_diagnostics(&result, &mut failed_diagnostics);
            }
        }

        for handle in handles {
            // A panicked worker shouldn't abort the whole process silently.
            let _ = handle.join();
        }

        if failures > 0 {
            return Err(DeftError::Compilation {
                failures,
                diagnostics: failed_diagnostics,
            });
        }
        Ok(objects)
    }

    /// Print diagnostics for a finished unit. A no-op in `--json` mode — the
    /// caller renders diagnostics from the returned `DeftError::Compilation`
    /// (or the success payload) as a single structured object instead.
    fn report_unit(&self, result: &UnitResult, idx: usize, total: usize) {
        if self.json {
            return;
        }
        if result.success {
            if self.verbose {
                eprintln!(
                    "  \x1b[2m[{idx}/{total}]\x1b[0m \x1b[32mok\x1b[0m {}",
                    result.source.display()
                );
            }
            // Surface warnings even on success.
            for d in &result.diagnostics {
                if d.severity != Severity::Error {
                    eprint!("{}", d.render());
                }
            }
            return;
        }

        eprintln!(
            "\x1b[1;31merror\x1b[0m: failed to compile \x1b[1m{}\x1b[0m",
            result.source.display()
        );
        if result.diagnostics.is_empty() {
            // Fall back to raw stderr if we couldn't parse anything structured.
            eprintln!("{}", result.raw_stderr.trim_end());
        } else {
            for d in &result.diagnostics {
                eprint!("{}", d.render());
            }
        }
    }

    /// Execute the link/archive step.
    ///
    /// `candidates` is ordered most-preferred first. On Unix there is always
    /// exactly one (`ar`); on Windows there may be two (`llvm-ar`, then
    /// `lib.exe`) since either could be the one actually installed. A
    /// candidate is skipped — not failed — only when the program itself can't
    /// be spawned; once a linker/archiver actually runs, its exit code is
    /// authoritative and reported as a real failure.
    fn run_link(&self, candidates: &[LinkCommand], kind: Crate) -> Result<()> {
        let mut last_spawn_err = None;

        for (i, link) in candidates.iter().enumerate() {
            if !self.quiet {
                let verb = match kind {
                    Crate::Executable => "Linking",
                    Crate::Library => "Archiving",
                };
                println!("\x1b[1;32m     {verb}\x1b[0m via {}", link.program);
            }
            if self.verbose {
                eprintln!(
                    "  \x1b[2m[engine]\x1b[0m {} {}",
                    link.program,
                    link.args.join(" ")
                );
            }

            let output = match Command::new(&link.program).args(&link.args).output() {
                Ok(out) => out,
                Err(source) => {
                    let is_last = i + 1 == candidates.len();
                    if !is_last {
                        if self.verbose {
                            eprintln!(
                                "  \x1b[2m[engine]\x1b[0m '{}' not found; trying next archiver",
                                link.program
                            );
                        }
                        last_spawn_err = Some(DeftError::CommandSpawn {
                            program: link.program.clone(),
                            source,
                        });
                        continue;
                    }
                    return Err(DeftError::CommandSpawn {
                        program: link.program.clone(),
                        source,
                    });
                }
            };

            if !output.status.success() {
                // Link errors also benefit from the diagnostics parser.
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let diags = parse_clang_diagnostics(&stderr);
                for d in &diags {
                    eprint!("{}", d.render());
                }
                if diags.is_empty() {
                    eprintln!("{}", stderr.trim_end());
                }
                return Err(DeftError::CommandFailed {
                    program: link.program.clone(),
                    code: output.status.code(),
                    stderr,
                });
            }
            return Ok(());
        }

        // Unreachable in practice — `link_command` never returns an empty
        // candidate list — but keeps the function total rather than panicking.
        Err(last_spawn_err
            .unwrap_or_else(|| DeftError::Config("no archiver/linker candidates available".into())))
    }
}

/// Fold one failed unit's diagnostics into the running list carried by
/// `DeftError::Compilation`. Prefers parsed `Error`-severity diagnostics;
/// falls back to a single synthetic entry built from raw stderr (or a
/// generic message) when clang's output couldn't be parsed at all.
fn collect_failure_diagnostics(result: &UnitResult, out: &mut Vec<CompileDiagnostic>) {
    let errors: Vec<&Diagnostic> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();

    if errors.is_empty() {
        let message = if result.raw_stderr.trim().is_empty() {
            "compilation failed with no diagnostic output".to_string()
        } else {
            result.raw_stderr.trim().to_string()
        };
        out.push(CompileDiagnostic {
            file: result.source.clone(),
            line: 0,
            column: 0,
            severity: "error",
            message,
        });
        return;
    }

    for d in errors {
        out.push(CompileDiagnostic {
            file: d.file.clone(),
            line: d.line,
            column: d.column,
            severity: "error",
            message: d.message.clone(),
        });
    }
}

/// Compile a single unit by invoking clang/clang++.
fn run_compile(unit: &CompileUnit) -> UnitResult {
    let driver = unit.language.driver();
    let output = Command::new(driver).args(&unit.args).output();

    match output {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let diagnostics = parse_clang_diagnostics(&stderr);
            UnitResult {
                source: unit.source.clone(),
                success: out.status.success(),
                diagnostics,
                raw_stderr: stderr,
            }
        }
        Err(e) => UnitResult {
            source: unit.source.clone(),
            success: false,
            diagnostics: vec![Diagnostic {
                severity: Severity::Error,
                file: unit.source.clone(),
                line: 0,
                column: 0,
                message: format!("could not launch '{driver}': {e} (is clang installed?)"),
                code: None,
                snippet: None,
            }],
            raw_stderr: String::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// Clang diagnostics parsing
// ---------------------------------------------------------------------------

/// Severity of a parsed diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    fn parse(s: &str) -> Option<Severity> {
        match s {
            "error" | "fatal error" => Some(Severity::Error),
            "warning" => Some(Severity::Warning),
            "note" => Some(Severity::Note),
            _ => None,
        }
    }

    fn color(self) -> &'static str {
        match self {
            Severity::Error => "\x1b[1;31m",   // bold red
            Severity::Warning => "\x1b[1;33m", // bold yellow
            Severity::Note => "\x1b[1;36m",    // bold cyan
        }
    }

    fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// A single structured diagnostic extracted from clang's stderr.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub message: String,
    /// e.g. `-Wunused-variable` if clang attributed the diagnostic to a flag.
    pub code: Option<String>,
    /// The source line clang echoed, if any.
    pub snippet: Option<String>,
}

impl Diagnostic {
    /// Render a clean, colorized, human-readable block for the terminal.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let color = self.severity.color();
        let reset = "\x1b[0m";

        // Header line: severity[code]: message
        out.push_str(color);
        out.push_str(self.severity.label());
        out.push_str(reset);
        if let Some(code) = &self.code {
            out.push_str(&format!("\x1b[2m[{code}]\x1b[0m"));
        }
        out.push_str(&format!(": {}\n", self.message));

        // Location line: --> file:line:col
        if self.line > 0 {
            out.push_str(&format!(
                "  \x1b[1;34m-->\x1b[0m {}:{}:{}\n",
                self.file.display(),
                self.line,
                self.column
            ));
        } else {
            out.push_str(&format!("  \x1b[1;34m-->\x1b[0m {}\n", self.file.display()));
        }

        // Optional source snippet.
        if let Some(snippet) = &self.snippet {
            out.push_str(&format!(
                "   \x1b[2m{:>4} |\x1b[0m {}\n",
                self.line, snippet
            ));
            if self.column > 0 {
                let pad = " ".repeat(self.column.saturating_sub(1));
                out.push_str(&format!(
                    "        \x1b[2m|\x1b[0m {}{}^{}\n",
                    pad, color, reset
                ));
            }
        }
        out.push('\n');
        out
    }
}

/// Parse clang/clang++ stderr into structured diagnostics.
///
/// Recognizes the canonical clang format:
///   `path/to/file.cpp:LINE:COL: severity: message [-Wsomething]`
/// followed optionally by a source snippet line and a caret line. Lines that
/// don't match a header are attached to the previous diagnostic as a snippet.
pub fn parse_clang_diagnostics(stderr: &str) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut lines = stderr.lines().peekable();

    while let Some(line) = lines.next() {
        if let Some(mut diag) = parse_header_line(line) {
            // Peek for a snippet line (next non-empty line that isn't another
            // header and isn't a bare caret).
            if let Some(next) = lines.peek() {
                let trimmed_next = next.trim_start();
                let is_header = parse_header_line(next).is_some();
                let is_caret = trimmed_next
                    .chars()
                    .all(|c| c == '^' || c == '~' || c == ' ')
                    && trimmed_next.contains('^');
                if !is_header && !is_caret && !next.trim().is_empty() {
                    diag.snippet = Some((*next).to_string());
                    lines.next();
                    // Consume a following caret line if present.
                    if let Some(after) = lines.peek() {
                        let t = after.trim_start();
                        let caret =
                            t.chars().all(|c| c == '^' || c == '~' || c == ' ') && t.contains('^');
                        if caret {
                            lines.next();
                        }
                    }
                }
            }
            diagnostics.push(diag);
        }
    }

    diagnostics
}

/// Try to parse a single clang header line into a `Diagnostic`.
fn parse_header_line(line: &str) -> Option<Diagnostic> {
    // Split off an optional trailing ` [-Wflag]` or ` [flag]` code.
    let (head, code) = match line.rfind(" [") {
        Some(idx) if line.ends_with(']') => {
            let code = &line[idx + 2..line.len() - 1];
            (&line[..idx], Some(code.to_string()))
        }
        _ => (line, None),
    };

    // Expect: file:line:col: severity: message
    // We split carefully because Windows paths could contain ':'. deft targets
    // Linux/BSD first, so a left-to-right scan on the known shape is fine.
    let mut parts = head.splitn(4, ':');
    let file = parts.next()?;
    let line_str = parts.next()?;
    let col_str = parts.next()?;
    let rest = parts.next()?; // " severity: message"

    let line_no: usize = line_str.trim().parse().ok()?;
    let col_no: usize = col_str.trim().parse().ok()?;

    let rest = rest.trim_start();
    // rest is "severity: message"; severity may be "fatal error".
    let sev_split = rest.find(": ")?;
    let sev_str = rest[..sev_split].trim();
    let message = rest[sev_split + 2..].trim().to_string();
    let severity = Severity::parse(sev_str)?;

    Some(Diagnostic {
        severity,
        file: PathBuf::from(file.trim()),
        line: line_no,
        column: col_no,
        message,
        code,
        snippet: None,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the artifact path for a package. Naming is platform-specific:
/// Windows wants `name.exe` / `name.lib`, Unix wants bare `name` /
/// `libname.a`.
fn artifact_path(
    profile_dir: &Path,
    kind: Crate,
    package_name: &str,
    output_name: Option<&str>,
) -> PathBuf {
    let name = output_name.unwrap_or(package_name);
    let filename = match kind {
        Crate::Executable => {
            if cfg!(target_os = "windows") {
                format!("{name}.exe")
            } else {
                name.to_string()
            }
        }
        Crate::Library => {
            if cfg!(target_os = "windows") {
                format!("{name}.lib")
            } else {
                format!("lib{name}.a")
            }
        }
    };
    profile_dir.join(filename)
}

/// Compute the object-file path for a source, mirroring its path under `src/`
/// to avoid collisions between same-named files in different directories.
fn object_path(obj_dir: &Path, layout: &Layout, source: &Path) -> PathBuf {
    let rel = source.strip_prefix(&layout.src).unwrap_or(source);
    let mut flat = rel.to_string_lossy().replace(['/', '\\'], "__");
    flat.push('.');
    flat.push_str(object_extension());
    obj_dir.join(flat)
}

/// `.obj` on Windows (MSVC/llvm-ar/lib.exe convention), `.o` everywhere else.
fn object_extension() -> &'static str {
    if cfg!(target_os = "windows") {
        "obj"
    } else {
        "o"
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Determine a sensible default parallelism when `-j` is not provided.
pub fn default_jobs() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Convenience: load a package's `[package]` table or error helpfully.
pub fn require_package(manifest: &Manifest, root: &Path) -> Result<Package> {
    manifest
        .package
        .clone()
        .ok_or_else(|| DeftError::ManifestParse {
            path: root.join("deft.toml"),
            message: "missing [package] table (name/version required to build)".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::Compiler;
    use crate::manifest::{CProfile, CppProfile};
    use std::sync::Mutex;

    /// Env vars are process-global; serialize the one test below that
    /// mutates `DEFT_HOME` so it can't race with itself across reruns.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn make_library_package(dir: &Path, name: &str) -> (Layout, Package) {
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.c"),
            "int deft_add(int a, int b) { return a + b; }\n",
        )
        .unwrap();
        let layout = Layout::discover(dir).unwrap();
        let package = Package {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            description: None,
            authors: Vec::new(),
            toolchain: None,
        };
        (layout, package)
    }

    /// End-to-end: a fresh library build must populate
    /// `~/.deft/cache/prebuilt/{hash}`, and an identical second build (even
    /// after the local `target/` is wiped) must be served from that cache
    /// instead of invoking the compiler again.
    #[test]
    fn build_package_populates_and_then_hits_the_global_cache() {
        if Command::new("clang").arg("--version").output().is_err() {
            eprintln!("skipping: clang not available in this environment");
            return;
        }
        let _guard = ENV_LOCK.lock().unwrap();

        let pid = std::process::id();
        let project = std::env::temp_dir().join(format!("deft-engine-cache-test-{pid}"));
        let home = std::env::temp_dir().join(format!("deft-engine-cache-home-{pid}"));
        let _ = fs::remove_dir_all(&project);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&project).unwrap();

        let prev_home = std::env::var("DEFT_HOME").ok();
        std::env::set_var("DEFT_HOME", &home);

        let (layout, package) = make_library_package(&project, "cachelib");
        let compiler = Compiler::new(
            CProfile::default(),
            CppProfile::default(),
            Vec::new(),
            &[],
            false,
        );
        let engine = Engine::new(1, false, true, false);
        let target_dir = project.join("target");

        let first = engine
            .build_package(&layout, &package, &compiler, &target_dir, None, false)
            .unwrap();
        assert!(
            !first.cache_hit,
            "first build should compile, not hit the cache"
        );

        // Wipe the local target dir so the second build can only succeed by
        // copying from the *global* cache, not by reusing a local leftover.
        fs::remove_dir_all(&target_dir).unwrap();

        let second = engine
            .build_package(&layout, &package, &compiler, &target_dir, None, false)
            .unwrap();
        assert!(
            second.cache_hit,
            "second build with identical inputs should hit the global cache"
        );
        assert!(second.path.is_file());

        match prev_home {
            Some(v) => std::env::set_var("DEFT_HOME", v),
            None => std::env::remove_var("DEFT_HOME"),
        }
        let _ = fs::remove_dir_all(&project);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn collect_failure_diagnostics_falls_back_to_raw_stderr_when_unparsed() {
        let result = UnitResult {
            source: PathBuf::from("src/main.c"),
            success: false,
            diagnostics: Vec::new(),
            raw_stderr: "some opaque linker-style failure".to_string(),
        };
        let mut out = Vec::new();
        collect_failure_diagnostics(&result, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, "error");
        assert!(out[0].message.contains("opaque linker-style failure"));
    }

    #[test]
    fn collect_failure_diagnostics_prefers_parsed_error_severity() {
        let result = UnitResult {
            source: PathBuf::from("src/main.c"),
            success: false,
            diagnostics: vec![
                Diagnostic {
                    severity: Severity::Warning,
                    file: PathBuf::from("src/main.c"),
                    line: 1,
                    column: 1,
                    message: "unused variable".into(),
                    code: None,
                    snippet: None,
                },
                Diagnostic {
                    severity: Severity::Error,
                    file: PathBuf::from("src/main.c"),
                    line: 2,
                    column: 3,
                    message: "undeclared identifier".into(),
                    code: None,
                    snippet: None,
                },
            ],
            raw_stderr: String::new(),
        };
        let mut out = Vec::new();
        collect_failure_diagnostics(&result, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 2);
        assert_eq!(out[0].message, "undeclared identifier");
    }

    #[test]
    fn artifact_path_matches_platform_convention() {
        let dir = PathBuf::from("/tmp/profile");
        let exe = artifact_path(&dir, Crate::Executable, "app", None);
        let lib = artifact_path(&dir, Crate::Library, "mylib", None);
        if cfg!(target_os = "windows") {
            assert_eq!(exe.file_name().unwrap(), "app.exe");
            assert_eq!(lib.file_name().unwrap(), "mylib.lib");
        } else {
            assert_eq!(exe.file_name().unwrap(), "app");
            assert_eq!(lib.file_name().unwrap(), "libmylib.a");
        }
        let overridden = artifact_path(&dir, Crate::Library, "mylib", Some("custom"));
        assert!(overridden
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("custom"));
    }
}
