# Architecture

This document describes how `deft` is put together internally: the design
philosophy behind its decisions, and the concrete mechanics of its hot path,
parallel compiler engine, language isolation, and archiver fallback chain.
It reflects the actual implementation in `src/`, not an aspirational design.

## Philosophy

`deft` is a build system and package manager for C and C++. Three commitments
shape every module:

**A manifest-driven mindset for C/C++.** The user-facing shape — `init`,
`build`, `run`, a `deft.toml` manifest, a `deft.lock` lockfile, `[features]`,
`[profile.*]` — gives C/C++ projects the declarative, manifest-and-lockfile
workflow that's normal in modern package ecosystems but rare in this space.
`deft.toml` declares package metadata, features, and compiler profiles;
`deft.lock` pins every dependency to an exact commit. Each package is a
single, strictly-typed unit that is either an executable or a library — there
is no broader "workspace member" concept beyond the `[workspace]` table
itself.

**Zero-dependency footprint.** `Cargo.toml` ([Cargo.toml](../Cargo.toml))
declares exactly three runtime dependencies: `clap` (CLI parsing), `serde`
(data model derive), and `toml` (manifest/lockfile format). There is no HTTP
client crate, no VCS crate, no CMake-parsing crate, and no async runtime.
Every place deft would normally reach for a library, it instead shells out to
a tool the host OS or toolchain already provides:

- Dependency fetching: `git` (the resolver wraps `Command::new("git")`,
  [resolver.rs](../src/resolver.rs)).
- Index syncing: `curl`/`wget` on Unix, PowerShell's `Invoke-WebRequest` on
  Windows ([resolver.rs](../src/resolver.rs) `fetch_to_file`).
- Reachability probing: `curl --head` ([resolver.rs](../src/resolver.rs)
  `probe_url`).
- Static archiving: `ar` / `llvm-ar` / `lib.exe`, never an archive-writing
  crate ([compiler.rs](../src/compiler.rs) `archiver_candidates`).
- Concurrency: bare `std::thread` + `std::sync`, never a thread-pool or
  async-executor crate ([engine.rs](../src/engine.rs) `compile_all`).

This keeps the deft binary itself small and fast to build, and keeps deft's
own supply chain trivially auditable.

**Strict layout enforcement.** deft does not glob for source files and does
not let a manifest declare a custom file list. `Layout::discover` in
[engine.rs](../src/engine.rs) recognizes exactly four canonical entry files,
in priority order:

1. `src/main.cpp` → executable, C++
2. `src/main.c` → executable, C
3. `src/lib.cpp` → library, C++
4. `src/lib.c` → library, C

If none exists, the build fails immediately with `DeftError::LayoutViolation`
before any compiler is invoked. This removes an entire class of "where did it
find that file" debugging that ad hoc build systems suffer from.

## Hot-Path Strategy

`deft build`'s defining performance goal is a near-instant invocation when the
environment is already healthy — the README's stated target is essentially
zero perceptible overhead beyond the actual compiler/linker work. This is
achieved by **trusting the environment** rather than verifying it.

Concretely, `cmd_build` in [main.rs](../src/main.rs) never probes for `clang`,
never checks `ar`, and never validates that `git`/`curl` exist before doing
real work. The only things it does before invoking the compiler are:

1. Locate `deft.toml` (`project_root`) — one `is_file` check.
2. Parse the manifest (`Manifest::load`) — one `read_to_string` + TOML parse.
3. Discover the layout (`Layout::assert_deft_standard`) — directory/file
   existence checks already required to find the entry point.
4. Resolve dependencies from the lockfile (no network I/O if the cache is
   already populated and the lock is honored).

All toolchain *health* checking — "is clang on PATH", "can clang actually
compile against this sysroot's headers", "is `ar` present", "is `$HOME` set"
— lives exclusively in `deft doctor` ([doctor.rs](../src/doctor.rs)), which a
healthy `deft build` never runs.

The connection between the two is `build_with_diagnostics` in
[main.rs](../src/main.rs):

```rust
fn build_with_diagnostics(args: BuildArgs, verbose: bool, quiet: bool) -> Result<BuildOutcome> {
    match cmd_build(args, verbose, quiet) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            // only on failure: print a note, then run `deft doctor::run`
            ...
        }
    }
}
```

A successful build pays zero cost for diagnostics. Only once a build has
*already failed* does deft pay for the comparatively expensive, exhaustive
`doctor` sweep (spawning `clang --version`, `ar --version`, `git --version`,
a real probe compile, etc.) — at that point the user is blocked anyway and
wants an explanation, so the cost is justified. This is the central
inference behind the "0.02s loop" framing: the fast path has a fixed, small
number of syscalls and zero speculative subprocess spawns; the slow,
diagnostic-heavy path is reserved for the already-broken case.

## Parallel Compilation Engine

All concurrency in [engine.rs](../src/engine.rs) is built from three standard
library primitives — `std::thread`, `std::sync::{Arc, Mutex}`, and
`std::sync::mpsc` — with no external thread-pool or job-server crate.

### Work queue

`Engine::compile_all` plans every translation unit up front into a
`Vec<CompileUnit>`, then moves it into a shared queue:

```rust
let queue: Arc<Mutex<VecDeque<CompileUnit>>> = Arc::new(Mutex::new(VecDeque::from(units)));
let (tx, rx) = mpsc::channel::<UnitResult>();
```

The worker count is `self.jobs.min(total).max(1)` — never more threads than
there are units to compile, and never zero. `self.jobs` itself defaults to
`default_jobs()`, which calls `std::thread::available_parallelism()`
(falling back to `1` if the OS query fails), and can be overridden by
`-j`/`--jobs`.

### Lock-holding minimization

Each worker thread runs a tight loop that holds the queue's mutex for the
shortest possible critical section — only the `pop_front()` itself:

```rust
let unit = {
    let mut q = match queue.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    q.pop_front()
};
let Some(unit) = unit else { break };
let result = run_compile(&unit); // lock already released
```

The lock is dropped (the block ends) *before* `run_compile` spawns the
`clang`/`clang++` child process and blocks on its output — the slow I/O bound
work happens entirely outside the critical section, so N workers can compile
N translation units fully in parallel with at most one thread ever blocked on
the queue mutex at a time, and only for a pointer-swap's worth of time.

A poisoned mutex (a previous panic while the lock was held) is recovered via
`poisoned.into_inner()` rather than propagating the panic — one slow/odd
translation unit should not take down the whole worker pool's ability to keep
draining the queue.

### Real-time streaming of results

Each worker sends its `UnitResult` (source path, success flag, parsed
`Diagnostic`s, raw stderr) over the `mpsc::Sender` as soon as that one unit
finishes — not batched, not buffered until the whole pool completes. The
main thread's `for result in rx` loop drains the channel as results arrive,
calling `report_unit` to print progress (`[idx/total] ok <path>` in verbose
mode, or error blocks immediately) interleaved with whatever other workers
are still compiling. The channel naturally closes once every worker thread
has dropped its cloned `Sender` and the orchestrator's own original `tx` was
explicitly dropped beforehand — so `for result in rx` terminates exactly when
all units are accounted for, with no explicit completion counter needed
beyond the `completed`/`total` figures used purely for display.

After draining results, every worker `JoinHandle` is `.join()`'d (ignoring
panics rather than propagating them — `let _ = handle.join();`), and the
function returns `DeftError::Compilation { failures }` if any unit failed,
or the full list of object file paths otherwise.

## Compiler Boundary Isolation

[compiler.rs](../src/compiler.rs) enforces a hard separation between C and
C++ at the **type level**, not just by convention:

```rust
pub enum Language {
    C,
    Cpp,
}
```

`Language::from_extension` is the single source of truth for recognizing a
translation unit's language (`.c` → `C`; `.cc`/`.cpp`/`.cxx`/`.c++`/`.cp` →
`Cpp`; anything else, including headers, → `None`, meaning "not a
translation unit").

The `Compiler` struct holds **both** profiles (`c_profile: CProfile`,
`cpp_profile: CppProfile`) but exposes two private methods, `c_args` and
`cpp_args`, each of which reads *only* its own profile field:

```rust
fn c_args(&self, source: &Path, object: &Path) -> Result<Vec<String>> {
    let p = &self.c_profile;   // never touches cpp_profile
    ...
}
fn cpp_args(&self, source: &Path, object: &Path) -> Result<Vec<String>> {
    let p = &self.cpp_profile; // never touches c_profile
    ...
}
```

`compile_unit` dispatches to exactly one of these based on
`Language::from_extension`, so there is no code path through which a C
file's compile command could pick up a C++-only flag (`-frtti`,
`-fexceptions`, `c++20` standard string) or vice versa. The two profile
structs, `CProfile` and `CppProfile` ([manifest.rs](../src/manifest.rs)),
are themselves physically distinct Rust structs with non-overlapping fields
beyond `standard`/`warnings`/`optimization`/`extra_flags`/`defines` — `rtti`
and `exceptions` exist only on `CppProfile`.

This isolation is also enforced at the *package* level by
`Layout::collect_sources` ([engine.rs](../src/engine.rs)): every package has
exactly one `entry_language` (decided by which of the four canonical entry
files exists), and any source file under `src/` whose language disagrees
with the entry language is collected into a `foreign` list and turned into a
hard `DeftError::LayoutViolation` rather than silently compiled. A deft
package is single-language, full stop — see [manifest.md](manifest.md) for
the directory-layout rules this produces.

Driver selection follows the same isolation: `Language::driver()` returns
`"clang"` for C and `"clang++"` for C++, and the link step
(`Compiler::link_command`) picks `clang++` over `clang` only when
`has_cpp` is true — i.e. when at least one compiled unit in the package was
C++, ensuring the C++ standard library is linked in exactly when needed and
never otherwise.

## Cross-Platform Archiver Fallback Chain

Producing a static library is the one step in deft where "the same tool
exists on every platform" is false: Unix systems have a single archiver
(`ar`) with one calling convention, but Windows toolchains may expose either
`llvm-ar` (Unix-style: `rcsD <archive> <objects...>`) or MSVC's `lib.exe`
(`/OUT:<archive> <objects...>`) — and which one is actually installed varies
by toolchain (LLVM-only vs. MSVC vs. MSYS2).

Rather than guessing or requiring a specific toolchain, `compiler.rs`
represents the archiving step as an **ordered list of candidates**:

```rust
pub struct LinkCommand {
    pub program: String,
    pub args: Vec<String>,
}
```

`Compiler::link_command` returns `Vec<LinkCommand>` for libraries (always
exactly one entry for executables, which only ever link with
`clang`/`clang++`). `archiver_candidates` builds that vector:

- **Unix** (`!cfg!(target_os = "windows")`): exactly one candidate,
  `ar rcsD <archive> <objects...>`.
- **Windows**: two candidates, most-preferred first —
  1. `llvm-ar rcsD <archive> <objects...>` (LLVM's `ar`-compatible archiver,
     present if the user has LLVM installed — likely, since deft already
     requires `clang`/`clang++` from the same distribution).
  2. `lib.exe /OUT:<archive> <objects...>` (MSVC's native librarian, present
     if Visual Studio Build Tools are installed instead).

The flags `rcsD` mirror GNU `ar`'s conventional create-archive invocation:
`r` (insert/replace members), `c` (create silently), `s` (write an index),
`D` (deterministic timestamps/UIDs for reproducible archives).

`Engine::run_link` ([engine.rs](../src/engine.rs)) is the consumer of this
list. It iterates the candidates in order and only **advances to the next
candidate when the program itself cannot be spawned** (`Command::output()`
returns an `Err` — the binary isn't on `PATH`):

```rust
let output = match Command::new(&link.program).args(&link.args).output() {
    Ok(out) => out,
    Err(source) => {
        let is_last = i + 1 == candidates.len();
        if !is_last {
            // log "not found; trying next archiver", remember the error, continue
            continue;
        }
        return Err(DeftError::CommandSpawn { ... }); // last candidate also missing
    }
};
```

Critically, once a candidate program *does* spawn, its exit code becomes
authoritative — a real archiving failure (bad objects, disk full, etc.) is
reported immediately as `DeftError::CommandFailed` with clang-diagnostic-style
formatting where applicable, and deft does **not** silently fall through to
the next candidate just because the first one that ran happened to fail. The
fallback chain exists purely to route around "tool not installed," never to
paper over a real build error.

The same archiver step also governs artifact naming
(`Engine::build_package`): library output is `lib<name>.a` on Unix versus
`<name>.lib` on Windows, and object files use the `.o` extension on Unix
versus `.obj` on Windows (`object_extension()`), matching each platform's
archiver convention.
