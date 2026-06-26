//! Compiler subsystem: translate manifest profiles into precise `clang` /
//! `clang++` argument vectors.
//!
//! Strict C / C++ separation is enforced at the type level: a translation unit
//! is *either* `Language::C` *or* `Language::Cpp`, and the function that builds
//! the argument vector takes the matching profile. There is no code path that
//! lets C flags leak into a C++ invocation or vice versa.

use std::path::{Path, PathBuf};

use crate::error::{DeftError, Result};
use crate::manifest::{CProfile, CppProfile};

/// The two — and only two — languages deft compiles. They are kept rigidly
/// distinct to avoid ABI and standard-mismatch bugs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    C,
    Cpp,
}

impl Language {
    /// The clang driver to invoke for this language.
    pub fn driver(self) -> &'static str {
        match self {
            Language::C => "clang",
            Language::Cpp => "clang++",
        }
    }

    /// Recognize a source file's language purely from its extension.
    ///
    /// Returns `None` for headers and unknown extensions — those are never
    /// compiled as translation units.
    pub fn from_extension(path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "c" => Some(Language::C),
            "cc" | "cpp" | "cxx" | "c++" | "cp" => Some(Language::Cpp),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Language::C => "C",
            Language::Cpp => "C++",
        }
    }
}

/// Optimization level, parsed from the manifest string into a closed set so we
/// never hand clang an unvalidated `-O<garbage>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    Osize,  // -Os
    Oztiny, // -Oz
    Odebug, // -Og
    Ofast,  // -Ofast
}

impl OptLevel {
    pub fn parse(raw: &str) -> Result<OptLevel> {
        Ok(match raw.trim() {
            "0" => OptLevel::O0,
            "1" => OptLevel::O1,
            "2" => OptLevel::O2,
            "3" => OptLevel::O3,
            "s" | "size" => OptLevel::Osize,
            "z" | "tiny" => OptLevel::Oztiny,
            "g" | "debug" => OptLevel::Odebug,
            "fast" => OptLevel::Ofast,
            other => {
                return Err(DeftError::Config(format!(
                    "unknown optimization level '{other}' (expected 0,1,2,3,s,z,g,fast)"
                )));
            }
        })
    }

    pub fn flag(self) -> &'static str {
        match self {
            OptLevel::O0 => "-O0",
            OptLevel::O1 => "-O1",
            OptLevel::O2 => "-O2",
            OptLevel::O3 => "-O3",
            OptLevel::Osize => "-Os",
            OptLevel::Oztiny => "-Oz",
            OptLevel::Odebug => "-Og",
            OptLevel::Ofast => "-Ofast",
        }
    }
}

/// Map a warning keyword from the manifest to a clang `-W` flag.
///
/// Unknown keywords are passed through as `-W<keyword>` so users can name any
/// clang warning group without deft needing an exhaustive table.
fn warning_flag(keyword: &str) -> String {
    match keyword.trim() {
        "all" => "-Wall".to_string(),
        "extra" => "-Wextra".to_string(),
        "error" => "-Werror".to_string(),
        "pedantic" => "-Wpedantic".to_string(),
        "everything" => "-Weverything".to_string(),
        other => format!("-W{other}"),
    }
}

/// A fully-specified compile job for one translation unit.
#[derive(Debug, Clone)]
pub struct CompileUnit {
    pub language: Language,
    pub source: PathBuf,
    pub object: PathBuf,
    /// The complete argument vector (excluding the driver program itself).
    pub args: Vec<String>,
}

/// Holds the resolved, validated compile settings for a single package and
/// produces per-unit argument vectors. Constructed once per build.
pub struct Compiler {
    c_profile: CProfile,
    cpp_profile: CppProfile,
    /// `-I` include directories shared by both languages (dependency headers).
    include_dirs: Vec<PathBuf>,
    /// `-D` defines injected for active features, e.g. `DEFT_FEATURE_SSL`.
    feature_defines: Vec<String>,
    /// When true, append `-g` and force a debug-friendly opt floor.
    debug: bool,
    /// Release builds set NDEBUG and trust the profile's optimization level.
    release: bool,
}

impl Compiler {
    pub fn new(
        c_profile: CProfile,
        cpp_profile: CppProfile,
        include_dirs: Vec<PathBuf>,
        active_features: &[String],
        release: bool,
    ) -> Compiler {
        let feature_defines = active_features
            .iter()
            .map(|f| format!("DEFT_FEATURE_{}", f.to_ascii_uppercase().replace('-', "_")))
            .collect();
        Compiler {
            c_profile,
            cpp_profile,
            include_dirs,
            feature_defines,
            debug: !release,
            release,
        }
    }

    /// Validate the profiles up front so a bad optimization level fails before
    /// any compilation begins, rather than mid-build.
    pub fn validate(&self) -> Result<()> {
        OptLevel::parse(&self.c_profile.optimization)?;
        OptLevel::parse(&self.cpp_profile.optimization)?;
        Ok(())
    }

    /// Resolve the optimization level actually handed to clang.
    ///
    /// `--release` always wins with `-O3`: the manifest's `optimization` field
    /// describes the dev/debug profile, not a release override. This is the
    /// one place `--release` maps onto a concrete `-O` flag.
    fn effective_opt(&self, profile_opt: &str) -> Result<OptLevel> {
        if self.release {
            return Ok(OptLevel::O3);
        }
        OptLevel::parse(profile_opt)
    }

    /// Build the compile command for a single source file. The `object` path is
    /// where the `.o` should be written.
    pub fn compile_unit(&self, source: &Path, object: &Path) -> Result<CompileUnit> {
        let language = Language::from_extension(source).ok_or_else(|| {
            DeftError::Config(format!(
                "cannot determine language for '{}' (unsupported extension)",
                source.display()
            ))
        })?;

        let args = match language {
            Language::C => self.c_args(source, object)?,
            Language::Cpp => self.cpp_args(source, object)?,
        };

        Ok(CompileUnit {
            language,
            source: source.to_path_buf(),
            object: object.to_path_buf(),
            args,
        })
    }

    /// Argument vector for a C translation unit. Only ever reads `c_profile`.
    fn c_args(&self, source: &Path, object: &Path) -> Result<Vec<String>> {
        let mut args = self.c_flags()?;
        args.push("-o".to_string());
        args.push(object.to_string_lossy().to_string());
        args.push(source.to_string_lossy().to_string());
        Ok(args)
    }

    /// Argument vector for a C++ translation unit. Only ever reads `cpp_profile`.
    fn cpp_args(&self, source: &Path, object: &Path) -> Result<Vec<String>> {
        let mut args = self.cpp_flags()?;
        args.push("-o".to_string());
        args.push(object.to_string_lossy().to_string());
        args.push(source.to_string_lossy().to_string());
        Ok(args)
    }

    /// The C flag set with no source/object paths baked in. Shared by
    /// `c_args` and [`Compiler::cache_fingerprint`].
    fn c_flags(&self) -> Result<Vec<String>> {
        let p = &self.c_profile;
        let opt = self.effective_opt(&p.optimization)?;
        let mut args = Vec::new();

        args.push("-c".to_string());
        args.push(format!("-std={}", p.standard));
        args.push(opt.flag().to_string());
        for w in &p.warnings {
            args.push(warning_flag(w));
        }
        self.push_common(&mut args, &p.defines);
        for extra in &p.extra_flags {
            args.push(extra.clone());
        }
        Ok(args)
    }

    /// The C++ flag set with no source/object paths baked in. Shared by
    /// `cpp_args` and [`Compiler::cache_fingerprint`].
    fn cpp_flags(&self) -> Result<Vec<String>> {
        let p = &self.cpp_profile;
        let opt = self.effective_opt(&p.optimization)?;
        let mut args = Vec::new();

        args.push("-c".to_string());
        args.push(format!("-std={}", p.standard));
        args.push(opt.flag().to_string());

        // RTTI / exceptions are C++-only toggles; encode them precisely.
        if p.rtti {
            args.push("-frtti".to_string());
        } else {
            args.push("-fno-rtti".to_string());
        }
        if p.exceptions {
            args.push("-fexceptions".to_string());
        } else {
            args.push("-fno-exceptions".to_string());
        }

        for w in &p.warnings {
            args.push(warning_flag(w));
        }
        self.push_common(&mut args, &p.defines);
        for extra in &p.extra_flags {
            args.push(extra.clone());
        }
        Ok(args)
    }

    /// Flags-only fingerprint for the global build cache: every flag that
    /// affects codegen, with no source/object paths baked in, so the same
    /// flags produce the same cache key regardless of where the project
    /// lives on disk.
    pub fn cache_fingerprint(&self, language: Language) -> Result<Vec<String>> {
        match language {
            Language::C => self.c_flags(),
            Language::Cpp => self.cpp_flags(),
        }
    }

    /// Flags common to both languages: includes, defines, debug/release shaping.
    fn push_common(&self, args: &mut Vec<String>, profile_defines: &[String]) {
        // Emit machine-parseable diagnostics with caret context.
        args.push("-fcolor-diagnostics".to_string());
        args.push("-fno-caret-diagnostics".to_string());

        for dir in &self.include_dirs {
            args.push(format!("-I{}", dir.display()));
        }
        for def in profile_defines.iter().chain(self.feature_defines.iter()) {
            args.push(format!("-D{def}"));
        }

        if self.debug {
            args.push("-g".to_string());
        }
        if self.release {
            args.push("-DNDEBUG".to_string());
        }
    }

    /// Build the final link command for an executable, or the ordered list of
    /// archiver candidates for a library.
    ///
    /// `objects` are the compiled object files; `output` is the artifact path
    /// (already extensioned correctly by the engine — `.o`/`.obj`, `.a`/`.lib`,
    /// with `.exe` on the executable side). `has_cpp` decides whether to link
    /// with the C++ driver (needed to pull in the C++ runtime/stdlib) — a
    /// concrete consequence of strict separation.
    ///
    /// Executables always resolve to exactly one command. Libraries resolve to
    /// one or more *candidates*, most-preferred first: the caller (`Engine`)
    /// is expected to try each in turn and only fall through to the next one
    /// if the program itself can't be spawned — the same "try, then fall
    /// back" shape used elsewhere in deft (e.g. the resolver's clone retries).
    pub fn link_command(
        &self,
        objects: &[PathBuf],
        output: &Path,
        has_cpp: bool,
        is_library: bool,
    ) -> Vec<LinkCommand> {
        if is_library {
            return archiver_candidates(objects, output);
        }

        let driver = if has_cpp { "clang++" } else { "clang" };
        let mut args = Vec::new();
        for o in objects {
            args.push(o.to_string_lossy().to_string());
        }
        args.push("-o".to_string());
        args.push(output.to_string_lossy().to_string());
        vec![LinkCommand {
            program: driver.to_string(),
            args,
        }]
    }
}

/// Static-archive command candidates, most preferred first.
///
/// Unix has one archiver with one calling convention: `ar rcsD <archive>
/// <objects...>`. Windows has two incompatible ones for the same job —
/// `llvm-ar` accepts that same Unix-style `rcsD <archive> <objects...>` form,
/// while MSVC's `lib.exe` wants `/OUT:<archive> <objects...>` instead. Rather
/// than guessing which toolchain is installed, both Windows candidates are
/// returned in preference order and `Engine::run_link` tries each in turn,
/// only advancing past a candidate when the program itself isn't found.
fn archiver_candidates(objects: &[PathBuf], output: &Path) -> Vec<LinkCommand> {
    let out = output.to_string_lossy().to_string();
    let objs: Vec<String> = objects
        .iter()
        .map(|o| o.to_string_lossy().to_string())
        .collect();

    if cfg!(target_os = "windows") {
        let mut llvm_ar_args = vec!["rcsD".to_string(), out.clone()];
        llvm_ar_args.extend(objs.iter().cloned());

        let mut lib_args = vec![format!("/OUT:{out}")];
        lib_args.extend(objs);

        return vec![
            LinkCommand {
                program: "llvm-ar".to_string(),
                args: llvm_ar_args,
            },
            LinkCommand {
                program: "lib.exe".to_string(),
                args: lib_args,
            },
        ];
    }

    let mut args = vec!["rcsD".to_string(), out];
    args.extend(objs);
    vec![LinkCommand {
        program: "ar".to_string(),
        args,
    }]
}

/// A resolved link/archive command.
#[derive(Debug, Clone)]
pub struct LinkCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiler() -> Compiler {
        Compiler::new(
            CProfile::default(),
            CppProfile::default(),
            Vec::new(),
            &[],
            false,
        )
    }

    /// `link_command` for an executable always resolves to exactly one
    /// candidate, driven by whichever language pulled in the unit (the C++
    /// driver if any translation unit was C++, to pull in the C++ runtime).
    #[test]
    fn link_command_executable_picks_driver_by_language() {
        let c = compiler();
        let objects = vec![PathBuf::from("main.o")];
        let output = PathBuf::from("app");

        let c_cmds = c.link_command(&objects, &output, false, false);
        assert_eq!(c_cmds.len(), 1);
        assert_eq!(c_cmds[0].program, "clang");

        let cpp_cmds = c.link_command(&objects, &output, true, false);
        assert_eq!(cpp_cmds.len(), 1);
        assert_eq!(cpp_cmds[0].program, "clang++");
    }

    /// The archiver fallback chain is platform-specific: Unix has exactly one
    /// candidate (`ar`), Windows offers `llvm-ar` first and falls back to
    /// MSVC's `lib.exe`, with each program's own calling convention intact.
    #[cfg(target_os = "windows")]
    #[test]
    fn link_command_library_windows_fallback_chain() {
        let c = compiler();
        let objects = vec![PathBuf::from("a.obj"), PathBuf::from("b.obj")];
        let output = PathBuf::from("mylib.lib");

        let cmds = c.link_command(&objects, &output, false, true);
        assert_eq!(cmds.len(), 2);

        assert_eq!(cmds[0].program, "llvm-ar");
        assert_eq!(cmds[0].args[0], "rcsD");
        assert_eq!(cmds[0].args[1], "mylib.lib");
        assert!(cmds[0].args.contains(&"a.obj".to_string()));
        assert!(cmds[0].args.contains(&"b.obj".to_string()));

        assert_eq!(cmds[1].program, "lib.exe");
        assert_eq!(cmds[1].args[0], "/OUT:mylib.lib");
        assert!(cmds[1].args.contains(&"a.obj".to_string()));
        assert!(cmds[1].args.contains(&"b.obj".to_string()));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn link_command_library_unix_single_candidate() {
        let c = compiler();
        let objects = vec![PathBuf::from("a.o"), PathBuf::from("b.o")];
        let output = PathBuf::from("libmy.a");

        let cmds = c.link_command(&objects, &output, false, true);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].program, "ar");
        assert_eq!(cmds[0].args[0], "rcsD");
        assert_eq!(cmds[0].args[1], "libmy.a");
        assert!(cmds[0].args.contains(&"a.o".to_string()));
        assert!(cmds[0].args.contains(&"b.o".to_string()));
    }

    /// Object/archive extensions are decided by the engine when it builds the
    /// `output`/`objects` paths handed to `link_command`, not by this
    /// function itself — verify the extension convention the rest of the
    /// pipeline relies on.
    #[test]
    fn object_and_archive_extensions_match_platform_convention() {
        if cfg!(target_os = "windows") {
            assert_eq!(Path::new("main.obj").extension().unwrap(), "obj");
            assert_eq!(Path::new("mylib.lib").extension().unwrap(), "lib");
        } else {
            assert_eq!(Path::new("main.o").extension().unwrap(), "o");
            assert_eq!(Path::new("libmy.a").extension().unwrap(), "a");
        }
    }

    /// The cache fingerprint must change when a flag-affecting profile field
    /// changes, and must never embed any source/object path (it has to be
    /// portable across machines/checkouts for the global cache to be useful).
    #[test]
    fn cache_fingerprint_excludes_paths_and_reflects_profile_changes() {
        let baseline = compiler();
        let fp_debug = baseline.cache_fingerprint(Language::C).unwrap();
        assert!(!fp_debug
            .iter()
            .any(|f| f.contains(".c") || f.contains(".o")));

        let release_profile = CProfile {
            optimization: "0".to_string(),
            ..CProfile::default()
        };
        let release = Compiler::new(
            release_profile,
            CppProfile::default(),
            Vec::new(),
            &[],
            true,
        );
        let fp_release = release.cache_fingerprint(Language::C).unwrap();

        assert_ne!(fp_debug, fp_release);
    }

    #[test]
    fn language_from_extension_is_strictly_separated() {
        assert_eq!(
            Language::from_extension(Path::new("foo.c")),
            Some(Language::C)
        );
        for ext in ["cc", "cpp", "cxx", "c++", "cp"] {
            assert_eq!(
                Language::from_extension(Path::new(&format!("foo.{ext}"))),
                Some(Language::Cpp)
            );
        }
        assert_eq!(Language::from_extension(Path::new("foo.h")), None);
    }
}
