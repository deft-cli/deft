//! `deft doctor` — environment and toolchain diagnostics.
//!
//! These checks are deliberately kept OUT of `deft build`'s hot path: a build
//! that already has a valid environment should not pay for probing it on
//! every invocation. Diagnostics run only when the user explicitly asks
//! (`deft doctor`) or when a build has just failed, at which point a slow,
//! thorough check is the right trade — the user is already stopped and wants
//! to know why.

use std::path::PathBuf;
use std::process::Command;

use crate::error::Result;

/// One diagnostic check and its outcome.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
    fix: Option<String>,
}

/// Run every diagnostic and print a human-readable report.
///
/// Returns `Ok(())` even when checks fail — `doctor` is a report, not a gate.
/// Callers that care about pass/fail can inspect the process exit code via
/// [`any_critical_failed`]-style logic if ever needed; today we keep it simple
/// and always exit 0 after printing, since the report itself is the value.
pub fn run(verbose: bool) -> Result<()> {
    let checks = vec![
        check_compiler("clang", "C"),
        check_compiler("clang++", "C++"),
        check_archiver(),
        check_git(),
        check_fetch_tool(),
        check_system_headers(),
        check_deft_home(),
    ];

    print_report(&checks, verbose);
    Ok(())
}

fn print_report(checks: &[Check], verbose: bool) {
    println!("\x1b[1;36mdeft doctor\x1b[0m — environment diagnostics\n");

    let mut passed = 0usize;
    for c in checks {
        if c.ok {
            passed += 1;
            println!("  \x1b[1;32m[ ok ]\x1b[0m {:<14} {}", c.name, c.detail);
        } else {
            println!("  \x1b[1;31m[fail]\x1b[0m {:<14} {}", c.name, c.detail);
            if let Some(fix) = &c.fix {
                println!("           \x1b[2mfix:\x1b[0m {fix}");
            }
        }
    }

    let failed = checks.len() - passed;
    println!();
    if failed == 0 {
        println!(
            "\x1b[1;32mAll {} checks passed.\x1b[0m Your environment is ready for `deft build`.",
            checks.len()
        );
    } else {
        println!(
            "\x1b[1;33m{passed} passed, {failed} failed.\x1b[0m Fix the items above, then re-run `deft doctor`."
        );
    }
    if verbose {
        eprintln!("  \x1b[2m[doctor]\x1b[0m ran {} check(s)", checks.len());
    }
}

/// Probe a clang driver for presence and version.
fn check_compiler(driver: &'static str, label: &str) -> Check {
    match Command::new(driver).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let first_line = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            Check {
                name: driver,
                ok: true,
                detail: first_line,
                fix: None,
            }
        }
        _ => Check {
            name: driver,
            ok: false,
            detail: format!("not found on PATH ({label} compiler required)"),
            fix: Some(install_hint_clang()),
        },
    }
}

fn check_archiver() -> Check {
    match Command::new("ar").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let first_line = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("ar")
                .to_string();
            Check {
                name: "ar",
                ok: true,
                detail: first_line,
                fix: None,
            }
        }
        _ => Check {
            name: "ar",
            ok: false,
            detail: "not found on PATH (required to archive static libraries)".to_string(),
            fix: Some(install_hint_binutils()),
        },
    }
}

fn check_git() -> Check {
    match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => Check {
            name: "git",
            ok: true,
            detail: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            fix: None,
        },
        _ => Check {
            name: "git",
            ok: false,
            detail: "not found on PATH (required to resolve gh: dependencies)".to_string(),
            fix: Some("install git (e.g. `sudo apt install git`, `brew install git`)".into()),
        },
    }
}

/// `curl` is preferred, `wget` and PowerShell's `Invoke-WebRequest` (on
/// Windows) are accepted fallbacks for package index syncing.
fn check_fetch_tool() -> Check {
    if cfg!(target_os = "windows") {
        return match Command::new("powershell")
            .args(["-NoProfile", "-Command", "$PSVersionTable.PSVersion"])
            .output()
        {
            Ok(out) if out.status.success() => Check {
                name: "powershell",
                ok: true,
                detail: "available for index sync (Invoke-WebRequest)".to_string(),
                fix: None,
            },
            _ => Check {
                name: "powershell",
                ok: false,
                detail: "not found (required to sync the package index on Windows)".to_string(),
                fix: Some("ensure PowerShell is installed and on PATH".into()),
            },
        };
    }

    if Command::new("curl")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return Check {
            name: "curl",
            ok: true,
            detail: "available for index sync".to_string(),
            fix: None,
        };
    }
    if Command::new("wget")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return Check {
            name: "wget",
            ok: true,
            detail: "available for index sync (curl not found, using fallback)".to_string(),
            fix: None,
        };
    }
    Check {
        name: "curl/wget",
        ok: false,
        detail: "neither curl nor wget found on PATH".to_string(),
        fix: Some(
            "install curl (e.g. `sudo apt install curl`, `brew install curl`) — \
             required for `deft sync` and dependency reachability probing"
                .into(),
        ),
    }
}

/// Confirm clang can actually find standard headers and produce an object
/// file end-to-end — the real-world test that "compiler present" doesn't
/// cover (a broken sysroot or missing libc headers still fails builds).
fn check_system_headers() -> Check {
    let dir = std::env::temp_dir();
    let probe_src = dir.join(format!("deft-doctor-{}.c", std::process::id()));
    let probe_obj = probe_src.with_extension("o");

    if std::fs::write(
        &probe_src,
        "#include <stdio.h>\nint main(void){return 0;}\n",
    )
    .is_err()
    {
        return Check {
            name: "headers",
            ok: false,
            detail: "could not write a probe file to the temp directory".to_string(),
            fix: Some("ensure the temp directory is writable".into()),
        };
    }

    let result = Command::new("clang")
        .args([
            "-c",
            probe_src.to_string_lossy().as_ref(),
            "-o",
            probe_obj.to_string_lossy().as_ref(),
        ])
        .output();

    let _ = std::fs::remove_file(&probe_src);
    let _ = std::fs::remove_file(&probe_obj);

    match result {
        Ok(out) if out.status.success() => Check {
            name: "headers",
            ok: true,
            detail: "stdio.h resolved and compiled cleanly".to_string(),
            fix: None,
        },
        Ok(out) => Check {
            name: "headers",
            ok: false,
            detail: "clang could not compile a trivial program".to_string(),
            fix: Some(format!(
                "check your sysroot/include paths; clang said: {}",
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .next()
                    .unwrap_or("")
            )),
        },
        Err(_) => Check {
            name: "headers",
            ok: false,
            detail: "skipped — clang itself is unavailable".to_string(),
            fix: Some(install_hint_clang()),
        },
    }
}

fn check_deft_home() -> Check {
    let home = match std::env::var("DEFT_HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => PathBuf::from(h).join(".deft"),
            _ => {
                return Check {
                    name: "deft home",
                    ok: false,
                    detail: "neither $DEFT_HOME nor $HOME is set".to_string(),
                    fix: Some("export HOME (or DEFT_HOME) so deft can locate its cache".into()),
                };
            }
        },
    };

    if home.is_dir() {
        Check {
            name: "deft home",
            ok: true,
            detail: home.display().to_string(),
            fix: None,
        }
    } else {
        Check {
            name: "deft home",
            ok: true,
            detail: format!("{} (will be created on first build)", home.display()),
            fix: None,
        }
    }
}

fn install_hint_clang() -> String {
    match std::env::consts::OS {
        "macos" => "install LLVM: `brew install llvm`".to_string(),
        "windows" => "install LLVM: `winget install LLVM.LLVM`".to_string(),
        _ => "install clang: `sudo apt install clang` (or your distro's equivalent)".to_string(),
    }
}

fn install_hint_binutils() -> String {
    match std::env::consts::OS {
        "macos" => "install binutils: `brew install binutils`".to_string(),
        "windows" => "install LLVM, which ships `llvm-ar`, or MSYS2 binutils".to_string(),
        _ => "install binutils: `sudo apt install binutils`".to_string(),
    }
}
