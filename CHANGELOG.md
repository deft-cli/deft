# Changelog

All notable changes to this project.

## [0.3.0] - 2026-06-26

### Added

- Global build cache at `~/.deft/cache/prebuilt/{hash}`: library packages
  (dependencies, and the root package when it's a library) whose sources,
  compiler flags, and target OS/arch hash identically to a previous build
  are copied straight from the cache, skipping the compile thread-pool
  entirely. Hashing is a small dependency-free module (`src/hash.rs`) built
  on `std::hash::Hasher`.
- `--json` global flag for `deft build` and `deft doctor`, emitting one
  compact, structured JSON object on stdout instead of human-readable text —
  build status/duration/cache-hit counts/compiler diagnostics, and an
  environment check matrix, respectively. Serialized with a small
  dependency-free encoder (`src/json.rs`) rather than `serde_json`.
- `deft vendor` subcommand: copies every dependency in `deft.lock` into a
  local `third_party/` tree. Once populated, `deft build` resolves
  dependencies from it directly — no git, no network, no global cache
  lookups — for fully offline/autonomous builds.
- `toolchain` field in `[package]` (e.g. `toolchain = "clang-18.1"`):
  `deft doctor` and the pre-build phase of `deft build` invoke the pinned
  compiler and abort with a descriptive error if its reported version
  doesn't match.

### Changed

- `deft doctor`'s report (human and `--json`) now includes a `toolchain`
  check when the current directory's manifest declares a pin; otherwise the
  report is unchanged.
- `build_dependencies` no longer takes an unused `Resolver` parameter.

## [0.2.1] - 2026-06-23

### Added

- CI workflow for testing across multiple OS environments.
- Tests for `sync` and `update` subcommands.

## [0.2.0] - 2026-06-22

### Added

- Full-featured CLI with `build`, `sync`, `update`, `doctor`, and `migrate` commands.
- Core build engine with parallel compilation.
- Dependency resolver and package index sync.
- Manifest and lockfile data models.
- C/C++ build argument generation.
- Centralized error handling.
- `migrate --from=cmake` command to import existing CMake projects.

### Changed

- Everything 

## [0.1.0] - 2026-06-16

- Initial release with core functionality.