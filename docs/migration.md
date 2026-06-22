# Migration Guide: `deft migrate --from=cmake`

This document covers deft's CMake-to-`deft.toml` migration tool, implemented
entirely in [migrate.rs](../src/migrate.rs). It is intentionally a "best
effort, never crash, always tell you what's left" tool rather than a full
CMake interpreter — the design rationale and every limitation below is
directly reflected in the source.

```
deft migrate [--from=cmake] [--path DIR]
```

Currently `--from` accepts only the literal value `"cmake"` (the default);
any other value returns `DeftError::Config` immediately. `--path` defaults to
`.` and must contain a `CMakeLists.txt`.

## `deft migrate --from=cmake`

### Zero-dependency token scanner mechanics

In keeping with deft's "no heavy deps" rule (see
[architecture.md](architecture.md#philosophy)), `migrate.rs` pulls in **no
CMake-parsing crate** — not even a generic grammar/parser-combinator library.
The entire extraction pipeline is plain `&str` scanning:

1. **Comment stripping.** `strip_comments` splits each line at the first `#`
   and keeps only the part before it:
   ```rust
   text.lines().map(|line| line.split('#').next().unwrap_or("")).collect::<Vec<_>>().join("\n")
   ```
   This intentionally does **not** handle CMake's rarer `#[[ ... ]]`
   bracket-comment syntax — the doc comment in the source explains the
   reasoning explicitly: "a naive tool errs on the side of flagging
   unfamiliar syntax rather than silently mis-parsing it." Content inside an
   unhandled bracket comment is left untouched and may surface as an
   unrecognized token rather than being silently consumed.

2. **Call extraction.** `extract_calls(text, command)` finds every invocation
   of a given CMake command (e.g. `"add_executable"`) and returns the raw
   text between its parentheses, one `String` per call site. It performs a
   **case-insensitive substring search** for `"<command>("` (CMake commands
   are conventionally case-insensitive), then hands the byte offset
   immediately following the open paren to the depth-counting parser below.

3. **Token unquoting.** `unquote(token)` strips a single matching pair of
   leading/trailing `"` or `'` quote characters from a token, leaving
   unquoted tokens (CMake variable names, bare library names) untouched.

Five CMake primitives are recognized this way:
`project`, `add_executable`, `add_library`,
`target_include_directories`, `target_link_libraries`. Anything else —
variables (`set(...)`, `${VAR}` expansion), control flow, generator
expressions — is not interpreted at all by the scanner; see
[Graceful Error Recovery](#graceful-error-recovery) for how that surfaces to
the user instead of being silently dropped.

### Parentheses depth-counting parser

CMake call arguments can themselves contain nested parens (most commonly in
`if(...)` conditions, though the migrator doesn't extract `if` at all today —
the depth counter is written generally enough to handle any future nested
call). `extract_calls`'s inner loop is a simple depth counter over raw bytes:

```rust
let mut depth = 1usize;
let mut end = start;
while end < bytes.len() && depth > 0 {
    match bytes[end] {
        b'(' => depth += 1,
        b')' => depth -= 1,
        _ => {}
    }
    end += 1;
}
if depth == 0 {
    calls.push(text[start..end - 1].to_string());
}
search_from = end.max(start + 1);
```

Starting depth is `1` (we've already consumed the call's own opening paren).
Each subsequent `(` increments depth, each `)` decrements it; the call's
argument text ends the moment depth returns to `0`, i.e. at the matching
close paren — this correctly balances nested parens within one call's
argument list. The known, accepted limitation (stated directly in the source
comment) is that **this scan does not understand parens inside quoted
strings** — a CMake argument like `"weird(unbalanced"` would desynchronize
the depth count. This is treated as acceptable for a tool whose entire
premise is "naive, best-effort, flags what it can't handle" rather than a
spec-complete CMake parser.

After each successful match, the search resumes from `end` (or `start + 1` if
somehow `end <= start`, guarding against infinite loops on a pathological
unbalanced-paren input) — so all call sites of the same command throughout
the file are collected, not just the first.

## Dominant Language Resolution

CMake places no single-language restriction on a target — a single
`add_executable` can legally mix `.c` and `.cpp` source files. deft's layout
model forbids this entirely (see
[manifest.md](manifest.md#directory-layout-standards)). Rather than making
migration an all-or-nothing failure for any project that mixes languages,
`parse_cmake` computes a **majority verdict** and partitions the rest:

```rust
let cpp_count = project.sources.iter().filter(|s| is_cpp_source(s)).count();
let c_count = project.sources.len() - cpp_count;
project.is_c = c_count > cpp_count;
```

`is_cpp_source` recognizes the same extension set as deft's own
`Language::from_extension` ([compiler.rs](../src/compiler.rs)): `cc`, `cpp`,
`cxx`, `c++`, `cp`. Every other recognized source extension (in practice,
`.c`) counts toward `c_count`.

**Tie-breaking.** The comparison is strict (`c_count > cpp_count`), so a tie
(equal counts, including the zero-sources case) resolves to `is_c = false`,
i.e. **C++ wins ties** — the source comment notes this matches `deft init`'s
own default behavior (no flags → C++ executable), keeping the two tools
consistent.

**Partitioning the minority.** Once the dominant language is decided, the
full source list collected from `add_executable`/`add_library` is split:

```rust
let dominant_is_cpp = !project.is_c;
let (dominant, conflicting): (Vec<String>, Vec<String>) = all_sources
    .into_iter()
    .partition(|s| is_cpp_source(s) == dominant_is_cpp);
project.sources = dominant;
project.conflicting_sources = conflicting;
```

`project.sources` (dominant-language files) are listed in the generated
manifest's migration notes as files to manually move into the appropriate
strict-layout entry file (`src/main.cpp`, etc. — deft has no "sources list"
in `deft.toml`, so even the dominant-language files require a manual move,
just without a language conflict to resolve first). `conflicting_sources`
(minority-language files) are excluded from the primary migration path
entirely and surfaced as TODOs — see below.

## Graceful Error Recovery

The tool's central guarantee, stated directly in the module doc comment, is
that `deft migrate` **must never abort on a mixed-language project** or on
any CMake construct it doesn't understand — every unmapped element is
preserved as an explicit, human-readable TODO in the generated `deft.toml`
rather than triggering a panic or hard error. Three categories of "unmapped"
input are handled this way:

**1. Unmapped link libraries.** CMake's `target_link_libraries` gives bare
library/target names (`mylib`, `pthread`, `Boost::filesystem`) that have no
automatic mapping to deft's `gh:user/lib` dependency shorthand — deft cannot
guess a GitHub repository from a bare name. Every captured name becomes a
commented-out, ready-to-uncomment-and-edit line:

```toml
[dependencies]
# TODO: map these CMake target_link_libraries to deft `gh:user/lib` deps
# (deft cannot infer a repository from a bare library name):
# "gh:<user>/mylib" = "x.y.z"  # was: mylib
```

**2. Unmapped (minority-language) source files.** The `conflicting_sources`
computed above are rendered as a dedicated TODO block at the end of the
manifest, naming the dominant/excluded languages, the exact file list, and
the structural reason (deft's one-language-per-package rule):

```toml
# TODO: Manually resolve mixed-language translation units
# deft enforces one language per package; C++ was chosen as the
# dominant language by source count. These C source(s) were excluded
# from this migration and need a plan (e.g. a sibling deft package):
#   - legacy/parser.c
#   - legacy/util.c
```

**3. Unmapped complex CMake logic.** `parse_cmake`'s final pass scans the
(comment-stripped) full text, case-insensitively, for five keyword markers
that indicate constructs the scanner does not interpret at all:
`foreach(`, `function(`, `macro(`, `while(`, `generate_export_header(`. Any
match is recorded in `complex_hits` and surfaced as a **runtime note**
(printed, not embedded in the TOML file itself, since there's no specific
location/value to anchor a TODO comment to):

```
note: CMakeLists.txt uses constructs deft does not parse (foreach, function).
Review the file manually for logic not captured above.
```

None of this — a mixed-language project, an unrecognized library name, or an
unparsed `foreach` loop — ever causes `deft migrate` to return an `Err` or
panic. The only conditions that actually fail the command outright are: an
unsupported `--from` value, a missing `CMakeLists.txt` at the resolved path,
or a `deft.toml` that already exists at the destination (overwrite
protection, mirroring `deft init`'s same check — see
[cli.md](cli.md#deft-init)).

## Diagnostic Feedback

`migrate::run` separates its output into two channels with different
visibility guarantees:

**Non-essential notices (`print_notices`, stdout, suppressed by `-q`).**
Printed only when `!quiet`:
- If any dominant-language sources were detected, a note listing them and
  the exact strict-layout entry file path they need to be manually moved
  into (deft has no sources list — moving files in is unavoidable manual
  work even for successfully-migrated projects):
  ```
  note: deft uses a strict layout — there is no "sources" list in deft.toml.
  Move/merge these detected sources into src/main.cpp by hand:
           - main.cpp
           - app.cpp
  ```
- If any of the five complex-construct keywords were detected, the
  `complex_hits` note shown above.

**The unmapped-source warning (`print_unmapped_warning`, stderr,
unconditional).** This is the one diagnostic deft prints **even under
`--quiet`** — the module doc comment explains why: "Mixed-language fallout is
always reported, even under `--quiet`: it lists exactly what the migration
could NOT map automatically, which is the one thing a user re-running this
non-interactively still needs to see." It only fires when
`conflicting_sources` is non-empty:

```
warning: mixed-language CMake project detected — C++ was chosen as the
dominant language (3 C++ vs. 2 C source file(s)). The following C file(s)
could not be mapped automatically and were left as TODOs in deft.toml:
           - legacy/parser.c
           - legacy/util.c
```

This asymmetry — informational layout guidance is quiet-suppressible,
structural data-loss-risk warnings are not — mirrors deft's general
philosophy that `-q` controls progress *noise*, never correctness-relevant
information (the same principle that keeps hard errors visible under `-q`
across every other command; see [cli.md](cli.md#global-constraints)).

After migration, the recommended next steps for a developer are, in order:
1. Move the listed dominant-language source files into the strict-layout
   entry path noted by `print_notices`.
2. Resolve the `# TODO` blocks for unmapped dependencies and
   conflicting-language sources in the generated `deft.toml`.
3. Manually review `CMakeLists.txt` for any logic guarded by the
   `complex_hits` keywords, since none of that control flow was translated.
4. Run `deft build` (or `deft doctor` first, if uncertain about the local
   toolchain) to validate the migrated package.
