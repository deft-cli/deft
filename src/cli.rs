//! Command-line interface definitions.
//!
//! Everything here is pure data: clap derive structs and enums describing the
//! surface of the `deft` binary. No logic lives here beyond what clap needs to
//! parse arguments. The runtime engine consumes these structures in `main.rs`.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// deft — a modern package manager and build system for C and C++.
#[derive(Parser, Debug)]
#[command(
    name = "deft",
    version,
    about = "A modern package manager and build system for C and C++.",
    long_about = "deft is a build system for C and C++ with strict \
                  project layout, Clang integration, and reproducible builds.",
    propagate_version = true
)]
pub struct Cli {
    /// Increase output verbosity (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Suppress non-essential output.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Emit machine-readable JSON instead of human-readable text.
    ///
    /// Honored by `build` and `doctor`; other commands accept the flag (it's
    /// global) but currently ignore it.
    #[arg(long, global = true)]
    pub json: bool,

    /// The subcommand to execute.
    #[command(subcommand)]
    pub command: Command,
}

/// All top-level subcommands deft understands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Compile the current package and all of its dependencies.
    Build(BuildArgs),

    /// Build (if needed) and then run the resulting executable.
    Run(RunArgs),

    /// Create a new deft package in the given directory (or current dir).
    Init(InitArgs),

    /// Re-resolve project dependencies and rewrite deft.lock.
    ///
    /// Touches only the current project's dependency graph: cache checkouts
    /// under `~/.deft/cache` and `deft.lock`. Never touches the package index
    /// (`~/.deft/deft-libs`) — see `Sync` for that.
    Update(UpdateArgs),

    /// Diagnose the local toolchain and environment (clang, ar, headers, ...).
    Doctor,

    /// Refresh the local package index (~/.deft/deft-libs) from the registry.
    ///
    /// Touches only that one flat-text index file via native OS fetch tools.
    /// Never resolves dependencies, never touches a project's `deft.lock` —
    /// see `Update` for that.
    Sync,

    /// Generate a starter deft.toml from an existing build system's config.
    Migrate(MigrateArgs),

    /// Vendor every dependency in deft.lock into a local third_party/ tree.
    ///
    /// Once third_party/ is populated, subsequent `deft build` runs resolve
    /// dependencies entirely from those local copies — no git, no network,
    /// no global cache lookups.
    Vendor(VendorArgs),
}

/// Arguments shared by `build` and (transitively) `run`.
#[derive(clap::Args, Debug, Clone)]
pub struct BuildArgs {
    /// Build with optimizations (uses the release configuration).
    #[arg(long)]
    pub release: bool,

    /// Override the output artifact name.
    #[arg(short = 'o', long = "output", value_name = "NAME")]
    pub output: Option<String>,

    /// Number of parallel compile jobs. Defaults to the number of CPUs.
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Path to the project root (defaults to the current directory).
    #[arg(long, value_name = "DIR")]
    pub manifest_path: Option<PathBuf>,

    /// Comma-separated list of features to activate.
    #[arg(long, value_name = "FEATURES", value_delimiter = ',')]
    pub features: Vec<String>,

    /// Do not activate the `default` feature set.
    #[arg(long)]
    pub no_default_features: bool,
}

/// Arguments for `deft run`.
#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Build configuration flags shared with `build`.
    #[command(flatten)]
    pub build: BuildArgs,

    /// Arguments forwarded verbatim to the executed binary (after `--`).
    #[arg(last = true, value_name = "ARGS")]
    pub bin_args: Vec<String>,
}

/// Arguments for `deft init`.
#[derive(clap::Args, Debug)]
pub struct InitArgs {
    /// Directory to initialize. Created if it does not exist.
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// Package name (defaults to the directory name).
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Initialize a library (`src/lib.cpp`) instead of an executable.
    #[arg(long, conflicts_with = "bin")]
    pub lib: bool,

    /// Initialize an executable (`src/main.cpp`). This is the default.
    #[arg(long)]
    pub bin: bool,

    /// Generate C sources/profile instead of C++.
    #[arg(long)]
    pub c: bool,
}

/// Arguments for `deft update`.
#[derive(clap::Args, Debug)]
pub struct UpdateArgs {
    /// Path to the project root (defaults to the current directory).
    #[arg(long, value_name = "DIR")]
    pub manifest_path: Option<PathBuf>,

    /// Only update this specific dependency (by package name).
    #[arg(value_name = "PACKAGE")]
    pub package: Option<String>,
}

/// Arguments for `deft vendor`.
#[derive(clap::Args, Debug)]
pub struct VendorArgs {
    /// Path to the project root (defaults to the current directory).
    #[arg(long, value_name = "DIR")]
    pub manifest_path: Option<PathBuf>,
}

/// Arguments for `deft migrate`.
#[derive(clap::Args, Debug)]
pub struct MigrateArgs {
    /// The build system to migrate from. Only "cmake" is supported today.
    #[arg(long, default_value = "cmake", value_name = "SYSTEM")]
    pub from: String,

    /// Directory containing the source build system's files (defaults to the
    /// current directory).
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,
}

impl Cli {
    /// Parse from the real process arguments.
    pub fn parse_args() -> Self {
        Cli::parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// `sync` and `update` are separate `Command` variants — parsing one
    /// invocation resolves to exactly one of them, never both, mirroring the
    /// fact that they touch entirely disjoint state (the global package
    /// index vs. one project's lockfile).
    #[test]
    fn sync_and_update_resolve_to_distinct_variants() {
        let sync = Cli::try_parse_from(["deft", "sync"]).expect("sync should parse");
        match sync.command {
            Command::Sync => {}
            other => panic!("expected Command::Sync, got {other:?}"),
        }

        let update = Cli::try_parse_from(["deft", "update"]).expect("update should parse");
        match update.command {
            Command::Update(_) => {}
            other => panic!("expected Command::Update, got {other:?}"),
        }

        // A single invocation only ever resolves one subcommand; trailing
        // tokens that look like another subcommand are rejected as stray
        // arguments rather than silently accepted.
        assert!(Cli::try_parse_from(["deft", "sync", "update"]).is_err());
    }

    #[test]
    fn update_accepts_an_optional_package_argument() {
        let cli = Cli::try_parse_from(["deft", "update", "mylib"]).unwrap();
        match cli.command {
            Command::Update(args) => assert_eq!(args.package, Some("mylib".to_string())),
            other => panic!("expected Command::Update, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["deft", "update"]).unwrap();
        match cli.command {
            Command::Update(args) => assert_eq!(args.package, None),
            other => panic!("expected Command::Update, got {other:?}"),
        }
    }

    #[test]
    fn sync_takes_no_positional_arguments() {
        assert!(Cli::try_parse_from(["deft", "sync", "extra"]).is_err());
    }

    /// `--json` is declared `global = true`, so it must parse both before
    /// and after the subcommand, on any command — not just `build`/`doctor`
    /// (those two are the only ones that currently *act* on it).
    #[test]
    fn json_flag_is_global_and_defaults_to_false() {
        let cli = Cli::try_parse_from(["deft", "build"]).unwrap();
        assert!(!cli.json);

        let before = Cli::try_parse_from(["deft", "--json", "build"]).unwrap();
        assert!(before.json);

        let after = Cli::try_parse_from(["deft", "doctor", "--json"]).unwrap();
        assert!(after.json);
    }

    #[test]
    fn vendor_parses_as_its_own_command_with_no_positional_arguments() {
        let cli = Cli::try_parse_from(["deft", "vendor"]).unwrap();
        match cli.command {
            Command::Vendor(args) => assert!(args.manifest_path.is_none()),
            other => panic!("expected Command::Vendor, got {other:?}"),
        }
        assert!(Cli::try_parse_from(["deft", "vendor", "extra"]).is_err());
    }

    /// The two variants also carry distinct, doc-comment-derived help text —
    /// the structural separation in `Command` is documented, not just an
    /// implementation detail invisible to `--help`.
    #[test]
    fn sync_and_update_have_distinct_help_text() {
        let cmd = Cli::command();

        let sync_about = cmd
            .find_subcommand("sync")
            .and_then(|c| c.get_about())
            .map(|s| s.to_string())
            .expect("sync subcommand should document itself");
        let update_about = cmd
            .find_subcommand("update")
            .and_then(|c| c.get_about())
            .map(|s| s.to_string())
            .expect("update subcommand should document itself");

        assert_ne!(sync_about, update_about);
        assert!(sync_about.to_lowercase().contains("index"));
        assert!(update_about.to_lowercase().contains("lock"));
    }
}
