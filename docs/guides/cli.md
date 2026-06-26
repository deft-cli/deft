# CLI Reference

Complete command and flag reference for the `deft` binary, derived from the
`clap` definitions in [cli.rs](../src/cli.rs) and the dispatch logic in
[main.rs](../src/main.rs).

```
deft [-v|--verbose]... [-q|--quiet] <COMMAND>
```

## Global Constraints

Two global flags are declared on the top-level `Cli` struct with
`global = true`, meaning they are accepted before *or* after the subcommand:

```rust
#[arg(short, long, action = clap::ArgAction::Count, global = true)]
pub verbose: u8,

#[arg(short, long, global = true, conflicts_with = "verbose")]
pub quiet: bool,
```

- **`-v` / `--verbose` (repeatable counting flag).** Uses
  `clap::ArgAction::Count`, so `verbose` is a `u8` incremented once per
  occurrence: `-v` → `1`, `-vv` → `2`, `-vvv` → `3`, etc. Internally, however,
  every call site only ever checks `cli.verbose > 0` (see `main()` in
  [main.rs](../src/main.rs): `let verbose = cli.verbose > 0;`) — there is
  currently no behavioral distinction between `-v` and `-vv`; both enable the
  same single verbose mode (extra `[engine]`/`[resolver]`/`[deft]` diagnostic
  lines prefixed in dim gray). The counting arity exists in the parser today
  primarily for forward compatibility with finer-grained verbosity levels.

- **`-q` / `--quiet`.** A plain boolean. Declared with
  `conflicts_with = "verbose"` — clap will reject any invocation that mixes
  `-q` with `-v`/`--verbose` at the argument-parsing stage, before deft's own
  code ever runs. Quiet mode suppresses the green/cyan progress lines
  (`Compiling`, `Linking`, `Locking`, `Updated`, `Created`, `Migrated`,
  `Syncing`, etc.) that every command prints by default, but does **not**
  suppress hard errors (printed via `eprintln!` to stderr regardless of
  `quiet`) or the unconditional `deft migrate` unmapped-source warning (see
  [migration.md](migration.md)).

Both flags are parsed once at the top of `main()` and threaded explicitly
through every command handler as `(verbose: bool, quiet: bool)` parameters —
there is no global/thread-local state.

- **`--json`.** Also `global = true`, so it parses before or after any
  subcommand. Only `deft build` and `deft doctor` currently act on it; other
  commands accept the flag without erroring but ignore it. When set, the
  command's entire human-readable output (the `Compiling`/`Linking`/`Finished`
  progress lines, the `doctor` table, etc.) is replaced by exactly one
  compact JSON object printed to stdout — see [`--json` Output](#--json-output)
  below for the payload shapes. Implemented with a small dependency-free
  encoder, [json.rs](../src/json.rs), rather than `serde_json`, to keep
  deft's three-dependency footprint unchanged (see
  [architecture.md](architecture.md)).

## Command Matrix

### `deft build`

```
deft build [--release] [-o NAME] [-j N] [--manifest-path DIR]
            [--features A,B,C] [--no-default-features]
```

| Flag | Mechanics |
|---|---|
| `--release` | Boolean. Passed through to `Compiler::new(..., release)` and `Engine::build_package(..., release)`. Two concrete effects: (1) `Compiler::effective_opt` **unconditionally returns `OptLevel::O3`**, ignoring whatever `optimization` string is set in `[profile.c]`/`[profile.cpp]` — release always means `-O3`, full stop, regardless of manifest config; (2) `push_common` appends `-DNDEBUG` and omits `-g`. Debug builds (`release = false`) do the opposite: honor the manifest's `optimization` field via `OptLevel::parse`, and always append `-g`. |
| `-o`, `--output NAME` | `Option<String>`. Overrides the artifact's base filename (before the platform-specific extension is applied: `.exe`/bare on Unix for executables, `.lib`/`lib*.a` for libraries). Defaults to the package name. |
| `-j`, `--jobs N` | `Option<usize>`. Resolved by the `jobs()` helper in [main.rs](../src/main.rs): `args.jobs.unwrap_or_else(default_jobs).max(1)`. This is the **clamping**: an explicit `-j` is floored to a minimum of `1` (so `-j 0` cannot spawn zero workers), and an absent `-j` falls back to `std::thread::available_parallelism()`. `Engine::new` applies a second floor (`jobs.max(1)`) and `compile_all` further clamps the *actual* worker count to `self.jobs.min(total)` — never more threads spawned than there are translation units to compile. |
| `--manifest-path DIR` | `Option<PathBuf>`. May point at a directory or directly at a `deft.toml` file (`project_root` strips the filename in the latter case). Defaults to the current working directory. Resolution fails fast with `LayoutViolation` if no `deft.toml` is found at the resolved root. |
| `--features A,B,C` | `Vec<String>`, comma-delimited (`value_delimiter = ','`). Unioned with the manifest's `default` feature set (unless suppressed) and transitively expanded — see [manifest.md](manifest.md#feature-flag-resolution). |
| `--no-default-features` | Boolean. Suppresses automatic inclusion of the `[features] default` set; explicitly-passed `--features` are still honored. |

**Toolchain pin.** If `[package] toolchain` is set (e.g. `"clang-18.1"`),
`build_single` validates it — invoking the named compiler and checking its
reported version — immediately after loading the manifest's `[package]`
table, before dependency resolution or any compilation. A mismatch aborts
the build with a descriptive error. See
[manifest.md](manifest.md#toolchain--pinning-the-active-compiler) for the
parsing/matching rules. Unset by default, so this costs nothing for projects
that don't opt in.

**Offline/vendored dependency resolution.** Before reaching for the
resolver at all, `build_single` checks whether `<root>/third_party/` exists
and has at least one entry. If so, dependencies are resolved entirely from
those local copies plus `deft.lock` metadata — no `git`, no network, no
global resolver cache lookups (`vendored_dependencies` in
[main.rs](../src/main.rs)). See [`deft vendor`](#deft-vendor) below for how
that directory gets populated.

**Profile mapping.** `build_single` loads `manifest.profile.c` /
`manifest.profile.cpp` (each `Option<CProfile>`/`Option<CppProfile>`,
defaulting via `.unwrap_or_default()` if the table is absent from
`deft.toml`) and constructs one `Compiler` for the whole package — the same
`Compiler` instance answers `compile_unit` for every translation unit,
dispatching internally to `c_args`/`cpp_args` per source file's detected
language (see [architecture.md](architecture.md#compiler-boundary-isolation)).

**Workspace builds.** If `manifest.is_workspace()` (a non-empty
`[workspace] members` list), `cmd_build` delegates to `build_workspace`,
which builds every member directory in declaration order via `build_single`
and returns the **last member's** `BuildOutcome` as the overall result —
there is no parallelism across workspace members, only within each member's
own translation units.

**Dependency build-before-root ordering.** Resolved dependencies are always
compiled — as libraries, regardless of whether their own layout would
otherwise resolve to an executable (`Layout { crate_kind: Crate::Library,
..dep_layout }` forcibly overrides the kind) — before the root package, so
their archives and `src/`/`include/` headers exist as `-I` include paths by
the time the root package's units are planned.

**Global build cache.** Before compiling any *library* package (the root
package when its entry is `src/lib.*`, and every dependency, which is always
built as a library — see above), `Engine::build_package` ([engine.rs](../src/engine.rs))
computes a deterministic cache key over the package's sources (path, content,
and mtime of each) and its resolved compiler flag fingerprint (standard,
optimization, warnings, defines — everything except source/object paths, so
the key is portable across checkouts), plus the target OS/arch
(`hash::package_key`, [hash.rs](../src/hash.rs)). If a static archive already
exists at `~/.deft/cache/prebuilt/{key}/lib{name}.a` (`.lib` on Windows), the
thread-pool is never spun up at all: the cached archive is copied straight
into the project's local `target/{debug,release}/` and a `Cache hit` line is
logged. A successful fresh build populates that same cache entry afterward
(best-effort — a write failure there never fails the build). Executables are
out of scope for this cache, since their output is project-specific rather
than a reusable artifact. Hashing uses only `std::hash::Hasher`
(`DefaultHasher`) — no extra crate.

### `deft run`

```
deft run [build flags...] [-- ARGS...]
```

```rust
pub struct RunArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    #[arg(last = true, value_name = "ARGS")]
    pub bin_args: Vec<String>,
}
```

`RunArgs` flattens the entire `BuildArgs` struct (`#[command(flatten)]`), so
every `deft build` flag documented above is also a valid `deft run` flag with
identical semantics — `deft run` is implemented as "build, then exec" with no
separate flag surface.

- **Validation against library crates.** After `build_with_diagnostics`
  returns a `BuildOutcome`, `cmd_run` checks `outcome.crate_kind !=
  Crate::Executable` and, if the package resolved to a `Library` (i.e. its
  entry point was `src/lib.cpp`/`src/lib.c`), returns
  `DeftError::LayoutViolation("\`deft run\` requires an executable
  (src/main.cpp or src/main.c)")` **after** the build has already succeeded —
  a library still gets fully compiled and archived; only the "now execute it"
  step is rejected.
- **Verbatim argument forwarding.** `#[arg(last = true)]` is clap's "greedy
  positional after `--`" marker: everything after a literal `--` token on the
  command line is captured into `bin_args` untouched — not reinterpreted as
  deft flags, not split/escaped/re-quoted. These are passed straight through
  to the child process via `Command::new(&outcome.artifact).args(&args.bin_args)`.
  This is why `deft run --release -- --release` correctly applies `--release`
  to the *build* once and forwards the literal string `--release` as the
  binary's own argv — clap stops parsing deft's own flags at the first bare
  `--`.
- The child's exit status is propagated: a non-zero exit causes
  `std::process::exit(status.code().unwrap_or(1))` from the deft process
  itself, so shell scripts checking `$?` after `deft run` see the *binary's*
  exit code, not deft's.

### `deft init`

```
deft init [PATH] [--name NAME] [--lib | --bin] [--c]
```

- `PATH` defaults to `.` (current directory); created with
  `create_dir_all` if absent, along with `PATH/src`.
- `--name` defaults to the canonicalized directory's file name (falling back
  to the literal string `"my_project"` if canonicalization fails, e.g. for a
  not-yet-existing relative path).
- **Language/kind selection.** `--lib` and `--bin` are mutually exclusive
  (`conflicts_with = "bin"` on `--lib`); `is_lib = args.lib && !args.bin`
  means the *default* (neither flag) is an executable. `--c` switches the
  generated language from C++ (default) to C. The four combinations select
  one of four hardcoded template pairs:

  | `--lib` | `--c` | Entry file | Template constant |
  |---|---|---|---|
  | no | no | `src/main.cpp` | `CPP_MAIN` (`#include <iostream>`, prints "Hello from deft!") |
  | no | yes | `src/main.c` | `C_MAIN` (`#include <stdio.h>`, `printf`) |
  | yes | no | `src/lib.cpp` | `CPP_LIB` (a `deft_add(int, int)` function) |
  | yes | yes | `src/lib.c` | `C_LIB` (same, C-flavored comment style) |

- **Overwrite protection.** Before writing, `cmd_init` checks
  `entry_path.exists()` and returns `DeftError::LayoutViolation("... already
  exists; refusing to overwrite")` — init never clobbers an existing entry
  file. The manifest (`deft.toml`) and `.gitignore` get the same treatment but
  via simple existence checks that silently skip writing rather than erroring
  (`if !manifest_path.exists() { ... }`), so re-running `deft init` in an
  already-initialized directory is a safe no-op for those two files as long
  as the entry source file itself is untouched.
- The generated manifest embeds a matching `[profile.c]` or `[profile.cpp]`
  block (`C_PROFILE`/`CPP_PROFILE` constants) so a freshly-`init`'d package
  builds immediately without further configuration.
- A `.gitignore` containing `/target` is written if absent.

### `deft doctor`

```
deft doctor
```

Takes no package-specific arguments — it diagnoses the *environment*, not a
particular project. Runs exactly seven checks, every one of them inline in
`doctor::run` ([doctor.rs](../src/doctor.rs)):

1. `clang --version` present (C compiler).
2. `clang++ --version` present (C++ compiler).
3. `ar --version` present (archiver — note this checks Unix `ar`
   specifically even on Windows, where the *build* path would actually try
   `llvm-ar`/`lib.exe`; `doctor`'s `ar` check is a baseline binutils probe).
4. `git --version` present (required for `gh:` dependency resolution).
5. A native fetch tool is present: `powershell` on Windows
   (`$PSVersionTable.PSVersion`), else `curl --version` falling back to
   `wget --version`.
6. **The end-to-end compilation probe.** `check_system_headers` writes a
   throwaway file to the OS temp directory, named uniquely per-process
   (`deft-doctor-<pid>.c`), containing exactly:
   ```c
   #include <stdio.h>
   int main(void){return 0;}
   ```
   then invokes `clang -c <probe>.c -o <probe>.o` and checks the exit status.
   This catches failures that "is clang on PATH" alone cannot — a broken
   sysroot, a missing or misconfigured libc headers package, or a clang
   installation that can't find its own resource directory. Both the probe
   source and the resulting object file are deleted (`remove_file`,
   best-effort) regardless of outcome.
7. `$DEFT_HOME` (or `$HOME/.deft` if unset) is locatable. This check always
   reports `ok: true` even when the directory doesn't exist yet — it only
   fails hard if *neither* `$DEFT_HOME` nor `$HOME` is set at all, since the
   directory itself is lazily created on first build/resolve.

**OS-aware fix suggestions.** Every failing check carries an optional
`fix: Option<String>` rendered under a `fix:` line in the report. Compiler and
archiver fixes branch on `std::env::consts::OS`:

```rust
fn install_hint_clang() -> String {
    match std::env::consts::OS {
        "macos" => "install LLVM: `brew install llvm`",
        "windows" => "install LLVM: `winget install LLVM.LLVM`",
        _ => "install clang: `sudo apt install clang` (or your distro's equivalent)",
    }
}
```

`install_hint_binutils` follows the same three-way branch
(`brew install binutils` / "install LLVM, which ships `llvm-ar`, or MSYS2
binutils" / `sudo apt install binutils`).

`doctor::run` always returns `Ok(())` — it is a **report, not a gate**: even
with every check failing, the process exit code stays `0` (the doctor module's
own doc comment is explicit: "Returns `Ok(())` even when checks fail —
`doctor` is a report, not a gate"). The pass/fail tally is purely a printed
summary line (`"{passed} passed, {failed} failed."`).

`doctor` is invoked two ways: explicitly via `deft doctor`, and automatically
(non-fatally — `let _ = doctor::run(verbose);`) by `build_with_diagnostics`
whenever a `deft build` or `deft run` invocation fails, right before deft
re-raises the original build error. See
[architecture.md](architecture.md#hot-path-strategy) for why this is split
out from the build's own hot path.

### `deft sync`

```
deft sync
```

Refreshes the **flat-text package index** at `~/.deft/deft-libs` (or
`$DEFT_HOME/deft-libs`) — the shorthand-to-URL mapping table used to resolve
`gh:user/lib`-style dependency keys that aren't already covered by the
built-in `gh:` → `https://github.com/<user>/<lib>.git` heuristic.

`cmd_sync` constructs a `Resolver` and calls `resolver.sync_index(quiet)`
([resolver.rs](../src/resolver.rs)). This is **strictly an index refresh** —
it loads no project manifest, resolves no dependency graph, and never reads
or writes a project's `deft.lock`. The doc comments in both
[cli.rs](../src/cli.rs) and [resolver.rs](../src/resolver.rs) call this out
explicitly to distinguish it from `deft update`.

**Zero-dependency manifest indexing.** The index's source URL defaults to:

```
https://raw.githubusercontent.com/deft-cli/deft-libs/main/deft-libs
```

overridable via the `DEFT_LIBS_URL` environment variable (for self-hosted or
air-gapped registries). The fetch itself uses only host-native tools, chosen
by `fetch_to_file`:

- **Windows** (`cfg!(target_os = "windows")`): `fetch_with_powershell` shells
  out to `powershell -NoProfile -NonInteractive -Command
  "Invoke-WebRequest -Uri '<url>' -OutFile '<dest>'"`.
- **Unix**: `fetch_with_curl_or_wget` tries
  `curl --silent --show-error --fail --location --max-time 30 -o <dest> <url>`
  first; if curl's `Command::status()` either errors (binary missing) or
  returns non-success, it falls back to
  `wget --quiet --timeout=30 -O <dest> <url>`. Only if *both* fail does it
  surface a `DeftError`.

**Atomicity.** The fetch writes to a sibling `deft-libs.tmp` file first, then
`fs::rename(&tmp, &dest)` performs the visible swap — a fetch that dies
partway through (network drop, disk full) never corrupts the live index,
since the rename is the only operation that touches the real `deft-libs`
path.

### `deft update`

```
deft update [PACKAGE] [--manifest-path DIR]
```

Re-resolves the **current project's** dependency graph from scratch and
rewrites `deft.lock` — the inverse operation to `deft sync` (which never
touches `deft.lock`) and complementary to `deft build` (which, by design,
*reads* the lock and never silently re-resolves on its own — see
[manifest.md](manifest.md#deftlock-spec)).

- **Full update** (`PACKAGE` omitted): `cmd_update` loads the existing lock
  only to pass as `pin = None` regardless of whether it exists — every
  dependency is resolved fresh, fetching current HEAD SHAs for each tag via
  `resolver.resolve_all(&manifest, None)`. Every entry in the rewritten lock
  reflects a fresh `git fetch`/`rev-parse HEAD`, even dependencies whose
  declared version string didn't change.
- **Scoped update** (`PACKAGE` given): the existing lock *is* passed as `pin`
  for the initial `resolve_all` call, so every dependency except the
  named target stays pinned to its previously-locked SHA. The named target is
  then explicitly re-resolved a second time with `pin = None`
  (`resolver.resolve_all(&manifest, None)`) and spliced into the result list,
  replacing the pinned entry. `package_name()` strips the shorthand down to
  its bare trailing path segment for the name comparison (e.g. `gh:user/lib`
  → `lib`), so `deft update lib` matches regardless of which shorthand prefix
  was used.
- **Dependency cache state.** Re-resolution does not necessarily mean
  re-cloning: `Resolver::ensure_cached` reuses an existing checkout under
  `~/.deft/cache/<name>-<tag>` if it already contains a `.git` directory,
  running only `git fetch --depth 1 origin tag <tag>` followed by
  `git checkout --quiet <tag>` rather than a fresh clone. A fresh clone only
  happens when the cache directory is absent or doesn't look like a git repo.
- The rewritten lockfile is written via the same atomic `.tmp` + `rename`
  pattern as `deft sync`'s index (see [manifest.md](manifest.md#deftlock-spec)).
- Non-quiet output prints one `Updated N dependenc{y,ies} in deft.lock` line
  followed by one `name vVERSION @ <10-char SHA prefix>` line per resolved
  dependency (`short_sha` truncates to the first 10 characters, or the full
  string if shorter).

### `deft vendor`

```
deft vendor [--manifest-path DIR]
```

Copies every dependency recorded in `deft.lock` into a local
`<root>/third_party/<name>/` tree, for complete offline autonomy — once that
directory is populated, every subsequent `deft build` resolves dependencies
from it directly (see [Offline/vendored dependency
resolution](#deft-build) above), with no `git`, no network, and no global
`~/.deft/cache` lookups at all.

`cmd_vendor` ([main.rs](../src/main.rs)):

1. Requires an existing `deft.lock` (`deft build` or `deft update` must have
   run at least once) — refuses with `DeftError::Config` otherwise, rather
   than silently re-resolving.
2. Resolves dependencies via `Resolver::resolve_all(&manifest, Some(&lock))`
   — the same *pinned* path `deft build` takes, so vendoring never drifts
   from what's actually locked.
3. For each resolved dependency, recursively copies its global-cache
   checkout (`dep.cache_path`) into `third_party/<name>/`, skipping any
   `.git` directory (`copy_tree_excluding_git`) — the vendored copy is a
   source snapshot, not a live git checkout. Any pre-existing
   `third_party/<name>/` is removed first, so re-running `deft vendor` is
   idempotent.
4. Non-quiet output prints one `Vendored <name> vVERSION -> <path>` line per
   dependency, then a `Finished vendoring N dependenc{y,ies}` summary.

### `--json` Output

`--json` (declared `global = true` on `Cli`, see [Global
Constraints](#global-constraints)) replaces a command's human-readable
output with one compact JSON object on stdout. Implemented by
[json.rs](../src/json.rs) — a closed, dependency-free `Json` enum
(`Null`/`Bool`/`Number`/`String`/`Array`/`Object`) with a `render()` method,
rather than pulling in `serde_json`.

**`deft build --json`.** `cmd_build_top_level` ([main.rs](../src/main.rs))
times the whole build, forces `quiet`/`json` through to every internal
`println!`/diagnostic-print call site (so no interim text reaches stdout —
see `Engine`'s `json` field in [engine.rs](../src/engine.rs)), and renders
exactly one of:

```json
{"status":"success","duration_ms":842,"cache_hits":2,"artifact":"target/debug/app","errors":[]}
```

```json
{"status":"failure","duration_ms":210,"cache_hits":0,"errors":[
  {"file":"src/main.c","line":4,"column":2,"severity":"error","message":"undeclared identifier 'foo'"}
]}
```

`cache_hits` sums every library package (dependencies, plus the root package
itself if it's a library) served from the global build cache instead of
recompiled — see the [Global build cache](#deft-build) section above.
`errors` carries the structured `CompileDiagnostic`s attached to
`DeftError::Compilation` when the failure came from the compiler; for any
other error kind (a layout violation, a missing manifest, a toolchain
mismatch, ...) it falls back to one synthetic entry built from the error's
`Display` text, with `file: null`.

**`deft doctor --json`.** `doctor::run` ([doctor.rs](../src/doctor.rs)) runs
the exact same checks as the human-readable report (see [`deft
doctor`](#deft-doctor) above, including the conditional `toolchain` check)
and renders:

```json
{"checks":[
  {"name":"clang","ok":true,"detail":"clang version 18.1.3","fix":null},
  {"name":"ar","ok":false,"detail":"not found on PATH ...","fix":"install binutils: ..."}
],"passed":1,"failed":1}
```

Every check always carries all four keys — `fix` is JSON `null`, never an
omitted key, when a check passed and has nothing to fix.
