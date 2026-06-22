<div align=center>
<h1>deft</h1>

<h6>A modern package manager and build system for C and C++, with strict
project-layout enforcement and deep Clang integration.</h6>

[![Deft Version](https://img.shields.io/badge/version-0.2.0-e.svg?style=for-the-badge&labelColor=000000&color=ffffff)](https://github.com/deft-cli/deft/releases/tag/v0.2.0)
[![Platform Support](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey.svg?style=for-the-badge&labelColor=000000&color=ffffff)](#)

</div>

## Why deft?

C and C++ tooling is fragmented. `deft` brings a familiar, manifest-driven
package-manager workflow while staying dependency-free itself — it shells out
to tools your system already has (`clang`, `git`, `curl`/`wget`/PowerShell,
`ar`/`llvm-ar`/`lib.exe`) instead of bundling an HTTP client, VCS library, or
archiver crate. See [docs/architecture.md](docs/architecture.md) for the full
rationale.

- **Strict project layout.** No globbing, no guessing. The entry point is
  exactly `src/main.cpp` / `src/main.c` (executables) or `src/lib.cpp` /
  `src/lib.c` (libraries). Missing it fails the build immediately.
- **Strict C / C++ separation.** A package is single-language. C and C++ are
  distinct enums in the build engine, and a package containing both source
  languages fails the build rather than silently mixing flags.
- **Manifest-driven Clang.** Optimization levels, warnings, language
  standard, RTTI, and exceptions all live in `deft.toml`. No messy flag
  strings.
- **Reproducible builds.** `deft.lock` pins every dependency to an exact git
  commit SHA, written atomically. `deft build` always honors the lock;
  `deft update` is the only command that rewrites it.
- **Parallel by default.** A `std::thread` + `Mutex<VecDeque>` + `mpsc` work
  queue compiles translation units across all cores (`-j` to tune), streaming
  diagnostics back as each unit finishes.
- **Human-readable diagnostics.** Clang's stderr is parsed and reformatted
  into clean, colorized terminal alerts.
- **Cross-platform static linking.** Archiving tries `ar` (Unix) or
  `llvm-ar` then `lib.exe` (Windows), falling through only when a tool is
  genuinely missing — see [docs/architecture.md](docs/architecture.md).

## Installation

Since Deft is currently in its early deployment phase, we distribute it directly via source compilation to ensure complete transparency and zero environmental friction.

```sh
cargo build --release
# binary at target/release/deft
```

Requires `clang`/`clang++` and `git` on `PATH`, plus an archiver (`ar` on
Unix; `llvm-ar` or `lib.exe` on Windows) and a fetch tool (`curl`/`wget` on
Unix, PowerShell on Windows). Run `deft doctor` after building to verify your
environment end-to-end, including a real probe compile against `<stdio.h>`.

## Commands

| Command       | Description                                                          |
| ------------- | --------------------------------------------------------------------- |
| `deft init`   | Scaffold a new package (`--lib`, `--bin`, `--c`, `--name`).            |
| `deft build`  | Compile the package (and its dependencies, and workspace members).    |
| `deft run`    | Build, then run the executable (`-- args` forwarded verbatim).         |
| `deft update` | Re-resolve dependencies and rewrite `deft.lock`.                      |
| `deft sync`   | Refresh the global package index (`~/.deft/deft-libs`) from the registry. |
| `deft doctor` | Diagnose the local toolchain (compiler, archiver, git, headers, ...). |
| `deft migrate`| Generate a starter `deft.toml` from an existing `CMakeLists.txt`.      |

Common flags: `--release`, `-o <name>`, `-j <N>`, `--features a,b`,
`--no-default-features`, `-v`, `-q`.

Full flag-by-flag mechanics (what `--release` actually overrides, how
`-j` is clamped, how `[-- ARGS...]` forwarding works, etc.) are documented in
[docs/cli.md](docs/cli.md).

## Quick start

```sh
deft init hello && cd hello
deft run
```

## The deft home

`deft` keeps global state under `~/.deft` (override with `$DEFT_HOME`):

- `~/.deft/deft-libs` — shorthand → URL mapping, one entry per line, refreshed
  by `deft sync`:
  ```
  gh:user/http_parser   https://github.com/user/http_parser.git
  ```
  `gh:user/lib` shorthands also resolve to GitHub automatically without an
  entry.
- `~/.deft/cache/` — global clone cache, keyed by `<name>-<tag>`, reused
  across projects and updates.

## License

Licensed under the MIT license — see [LICENSE.md](LICENSE.md).
