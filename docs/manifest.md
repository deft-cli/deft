# Manifest & Lockfile Specification

The absolute structural specification for `deft.toml` and `deft.lock`, drawn
directly from the serde data model in [manifest.rs](../src/manifest.rs) and
the layout rules in [engine.rs](../src/engine.rs).

## `deft.toml` Spec

The root deserialization target is `Manifest`:

```rust
pub struct Manifest {
    pub workspace: Option<Workspace>,
    pub package: Option<Package>,
    pub features: BTreeMap<String, Vec<String>>,
    pub profile: Profiles,
    pub dependencies: BTreeMap<String, Dependency>,
}
```

Every top-level table is optional at the parse level (`#[serde(default)]` on
all but `package`, and `package` itself is `Option<Package>`) — a manifest
with none of these tables still parses successfully. `package` is only
required at the point a package is actually *built*: `require_package` in
[engine.rs](../src/engine.rs) turns a missing `[package]` table into
`DeftError::ManifestParse { message: "missing [package] table (name/version
required to build)" }`. This split lets a workspace root manifest declare
only `[workspace]` with no `[package]` of its own.

### `[package]`

```toml
[package]
name = "my_project"
version = "0.2.0"
description = "optional"
authors = ["optional", "list"]
```

```rust
pub struct Package {
    pub name: String,         // required
    pub version: String,      // required
    pub description: Option<String>,  // default: None
    pub authors: Vec<String>,         // default: []
}
```

`name` and `version` have no `#[serde(default)]` — both are mandatory once a
`[package]` table is present at all.

### `[workspace]`

```toml
[workspace]
members = ["app", "lib/core"]
```

```rust
pub struct Workspace {
    pub members: Vec<String>,  // default: []
}
```

`Manifest::is_workspace()` returns `true` only when `workspace` is `Some`
**and** `members` is non-empty — a `[workspace]` table with an empty or
absent `members` list is treated as not a workspace at all. Each member path
is resolved relative to the workspace root and must itself be a complete
deft-standard package (own `deft.toml`, own `src/` layout) — see
[cli.md](cli.md#deft-build) for the build-order semantics.

### `[features]`

```toml
[features]
default = ["ssl"]
ssl = ["tls"]
tls = []
```

Modeled directly as `BTreeMap<String, Vec<String>>` — there is no dedicated
`Feature` struct. Each key is a feature name; each value is the list of other
feature names it *implies*. The conventional `default` key, if present, is
the seed set activated unless `--no-default-features` is passed. See
[Feature Flag Resolution](#feature-flag-resolution) below for the expansion
algorithm.

### `[profile.c]` and `[profile.cpp]`

```toml
[profile.c]
standard = "c17"           # default: "c17"
warnings = ["all", "extra"] # default: []
optimization = "0"          # default: "0"
extra_flags = []             # default: []
defines = []                  # default: []

[profile.cpp]
standard = "c++20"          # default: "c++20"
rtti = false                  # default: true
exceptions = true             # default: true
warnings = ["all", "extra"] # default: []
optimization = "0"          # default: "0"
extra_flags = []             # default: []
defines = []                  # default: []
```

`Profiles` is `{ c: Option<CProfile>, cpp: Option<CppProfile>, ...
#[serde(default)] }` — a manifest may declare neither, either, or both
profile tables. A package only needs the profile matching its own entry
language; an absent table is filled in with `CProfile::default()` /
`CppProfile::default()` at build time (`manifest.profile.c.clone()
.unwrap_or_default()` in [main.rs](../src/main.rs)), so omitting `[profile.c]`
entirely from a C++ package's manifest is normal and harmless.

`optimization` is a free-form **string** at the manifest level, validated
lazily by `OptLevel::parse` ([compiler.rs](../src/compiler.rs)) only at build
time — accepted values are `"0"`, `"1"`, `"2"`, `"3"`, `"s"`/`"size"`,
`"z"`/`"tiny"`, `"g"`/`"debug"`, `"fast"`. An unrecognized value produces
`DeftError::Config` *before* any compilation begins
(`Compiler::validate()` is called up front in `Engine::build_package`), not
mid-build.

`warnings` entries map through `warning_flag()` ([compiler.rs](../src/compiler.rs)):
`"all"` → `-Wall`, `"extra"` → `-Wextra`, `"error"` → `-Werror`,
`"pedantic"` → `-Wpedantic`, `"everything"` → `-Weverything`; any other
string `kw` passes through verbatim as `-W<kw>`, so any clang warning group
can be named without deft needing an exhaustive table.

`extra_flags` is an escape hatch: each string is appended to the clang
invocation verbatim, after all other deft-managed flags, normally left empty.

`defines` entries become `-D<entry>` flags, alongside the `-D` flags deft
synthesizes itself for active features (see below) — both sets are merged in
`push_common`.

`rtti` and `exceptions` exist **only** on `CppProfile` — there is no C
equivalent, since these are C++ language features. `false` maps to
`-fno-rtti`/`-fno-exceptions`; `true` (the default for both) maps to
`-frtti`/`-fexceptions`. This field-level asymmetry between `CProfile` and
`CppProfile` is itself part of deft's compiler boundary isolation — see
[architecture.md](architecture.md#compiler-boundary-isolation).

### `[dependencies]`

```toml
[dependencies]
"gh:user/http_parser" = "1.5"
"gh:another/ssl" = { version = "2.1", features = ["ssl"], tag = "v2.1.0" }
```

Keys are shorthand strings (conventionally `gh:user/lib` for GitHub, resolved
through `Resolver::map_shorthand` — see [resolver.rs](../src/resolver.rs)).
Values deserialize into `Dependency`:

```rust
pub struct Dependency {
    pub version: String,
    pub features: Vec<String>,
    pub tag: Option<String>,
}
```

**Untagged shorthand-vs-table deserialization.** `Dependency` implements
`Deserialize` by hand (not via derive) specifically to accept either a bare
string or a detailed table in the same map value position:

```rust
#[derive(Deserialize)]
#[serde(untagged)]
enum Raw {
    Simple(String),
    Detailed {
        version: String,
        #[serde(default)] features: Vec<String>,
        #[serde(default)] tag: Option<String>,
    },
}
```

serde's `#[serde(untagged)]` attempts each enum variant in declaration order
without a discriminant field, so a plain TOML string value
(`"gh:user/lib" = "1.5"`) matches `Raw::Simple` (it's just a string), while
an inline table (`{ version = "2.1", features = [...] }`) matches
`Raw::Detailed` (it has the shape of a map with a required `version` key).
Both variants are then normalized into the same `Dependency` struct — `Simple`
gets empty `features` and `tag: None`. This lets a manifest author write the
terse form for the common case (just a version) and only reach for the table
form when they need `features` or an explicit `tag` override (used when the
git tag differs from the semantic version string).

## Feature Flag Resolution

`Manifest::resolve_features(requested: &[String], no_default: bool) ->
Vec<String>` computes the final, transitively-expanded set of active
features and is the single place feature logic lives:

```rust
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
```

**Algorithm:** this is a depth-first, stack-based transitive closure over the
implication graph encoded by `[features]`. The seed stack is the union of
(a) the `default` feature's list, unless `--no-default-features` was passed,
and (b) the `--features a,b,c` list from the CLI. The loop pops one feature
name at a time; if it's already in `enabled`, it's skipped (cycle/duplicate
protection — the graph is allowed to have diamonds or even cycles without
infinite-looping, since each name is only ever pushed onto `enabled` once);
otherwise it's recorded, and *its own* implied features (its entry in the
`[features]` map) are pushed onto the stack to be expanded in turn. The final
result is sorted and deduplicated for deterministic, order-independent
output.

**Compile-time macros.** Each member of the resolved feature set becomes a
preprocessor define via `Compiler::new`:

```rust
let feature_defines = active_features
    .iter()
    .map(|f| format!("DEFT_FEATURE_{}", f.to_ascii_uppercase().replace('-', "_")))
    .collect();
```

So a feature named `ssl-support` becomes `-DDEFT_FEATURE_SSL_SUPPORT`,
injected into every translation unit's compile command alongside the
profile's own `defines` (`push_common` in
[compiler.rs](../src/compiler.rs)). There is no per-feature include path or
source-file gating beyond this macro — conditional compilation inside
sources (`#ifdef DEFT_FEATURE_SSL_SUPPORT`) is the mechanism by which a
feature actually changes behavior.

Dependencies currently resolve their **own** default feature set
independently (`dep_manifest.resolve_features(&[], false)` in
[main.rs](../src/main.rs) `build_dependencies`) — a consuming package's
`--features` selection does not yet propagate into its dependencies' builds.

## `deft.lock` Spec

```rust
pub struct Lockfile {
    #[serde(rename = "dependency")]
    pub dependencies: Vec<LockedDependency>,
}

pub struct LockedDependency {
    pub name: String,        // bare package name
    pub source: String,      // "git+<url>"
    pub checksum: String,    // exact resolved commit SHA
    pub version: String,     // requested version/tag string
    pub dependencies: Vec<String>,  // direct dependency names (graph edges)
}
```

On disk this serializes (via `#[serde(rename = "dependency")]`) as a flat
array of TOML tables:

```toml
# This file is auto-generated by deft.
# It records exact resolved versions for reproducible builds.
# Do not edit by hand; run `deft update` to regenerate.

[[dependency]]
name = "http_parser"
source = "git+https://github.com/user/http_parser.git"
checksum = "a1b2c3d4e5f6..."
version = "1.5"
dependencies = []
```

Entries are sorted by `name` before serialization (`Lockfile::save` sorts a
cloned copy), so the file's diff stays stable across re-locks regardless of
the manifest's `[dependencies]` declaration order (the manifest side is
already a `BTreeMap`, so it's naturally key-sorted; the explicit lock-side
sort makes the *lock's* ordering independent of any future change to that).

**Role in reproducible builds.** `deft build` calls
`resolver.resolve_all(manifest, existing_lock.as_ref())`. When a lock entry
exists for a dependency *and* its recorded `version` still matches the
manifest's currently-declared version, `Resolver::resolve_one` takes the
"reproducible path": it hard-resets the cached checkout to the locked
`checksum` via `git checkout --quiet <sha>` (fetching `--unshallow` first if
the SHA isn't present locally) rather than reading the tag's current HEAD.
Only when there is no lock entry, or the manifest's version string has
changed, does resolution fall through to `head_sha()` (fresh HEAD of the
requested tag). This is the mechanism that makes `deft build` immutable
across machines and over time as long as `deft.lock` is committed: two
checkouts of the same repository with the same `deft.lock` resolve every
dependency to the exact same commit, never a possibly-moved tag.

If no lock exists yet, the very first successful `deft build` writes one
(`build_single` in [main.rs](../src/main.rs)): `if existing_lock.is_none() &&
!resolved.is_empty() { ... lock.save(root)?; }` — first-build lock creation is
implicit; subsequent builds never silently rewrite the lock on their own
(only `deft update` does that intentionally).

**Atomic write pattern.** `Lockfile::save`:

```rust
let tmp = path.with_extension("lock.tmp");
fs::write(&tmp, format!("{header}{body}")).path_ctx(&tmp)?;
fs::rename(&tmp, &path).path_ctx(&path)?;
```

The full serialized contents (including the three-line auto-generated-file
header comment) are written to a sibling `deft.lock.tmp` first, and only the
final `fs::rename` touches the real `deft.lock` path. `rename` within the
same filesystem is atomic at the OS level, so a process interrupted mid-write
(crash, killed build, disk full) can only ever leave a stale-but-intact
`.tmp` file behind — the live `deft.lock` is either the previous complete
version or the new complete version, never a half-written file. This exact
pattern (write `.tmp`, then `rename`) is reused for the `~/.deft/deft-libs`
package index in `deft sync` — see [cli.md](cli.md#deft-sync).

## Directory Layout Standards

`Layout::discover` ([engine.rs](../src/engine.rs)) enforces the **four
canonical entry file priority queue** — deft never globs `src/` looking for
"a" main file; it checks for exactly these four paths, in this exact
precedence order, and uses the first one found:

| Priority | Path | Crate kind | Language |
|---|---|---|---|
| 1 | `src/main.cpp` | Executable | C++ |
| 2 | `src/main.c` | Executable | C |
| 3 | `src/lib.cpp` | Library | C++ |
| 4 | `src/lib.c` | Library | C |

An executable entry (`main.*`) always wins over a library entry
(`lib.*`) if, somehow, both exist in the same `src/` — the doc comment in
[engine.rs](../src/engine.rs) states the rationale plainly: "a package with
`main.*` is runnable." If none of the four exist, the build fails immediately
with a `DeftError::LayoutViolation` enumerating all four expected paths — this
check happens before the manifest's `[profile.*]` is even consulted, since
there is nothing to compile without a discovered entry point.

**Strict single-language enforcement.** Once the entry file fixes the
package's `entry_language`, `Layout::collect_sources` recursively walks
`src/` and partitions every recognized source file (anything
`Language::from_extension` returns `Some` for) into either `sources`
(matching `entry_language`) or `foreign` (the other language). Headers and
unrecognized extensions are silently skipped — they're not translation units
either way. If `foreign` is non-empty, the **entire build is rejected** with
a `DeftError::LayoutViolation` naming the offending language, the count of
foreign files, and one example path:

```
strict C/C++ separation violated: this is a Cpp package but found 2 C
source file(s) (e.g. 'src/legacy/util.c'). A deft package is single-language.
```

There is no per-file override or escape hatch for this rule within a single
package — a project needing both languages must be split into separate
deft-standard packages (e.g. a workspace with one C member and one C++
member), each independently satisfying the four-file/single-language rule.
See [migration.md](migration.md#dominant-language-resolution) for how
`deft migrate` handles pre-existing mixed-language CMake projects under this
same constraint.
