use std::env;
use std::process::Command;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_help();
        return;
    }

    match args[1].as_str() {
        "version" | "-v" | "--version" => {
            println!("Version: {}", env!("CARGO_PKG_VERSION"));
        }
        "help" | "-h" | "--help" => {
            print_help();
        }
        "build" => {
            run_build();
        }
        _ => {
            println!("Error: Unknown command '{}'.", args[1]);
            print_help();
        }
    }
}

fn print_help() {
    println!("Usage: deft <command>");
    println!("\nAvailable commands:");
    println!("  version     Show the current version");
    println!("  build       Run the build command");
    println!("  help        Show this help message");
}

fn run_build() {
    let current_dir = match env::current_dir() {
        Ok(path) => path,
        Err(e) => {
            println!("Error getting current directory: {}", e);
            return;
        }
    };

    let folder_name = match current_dir.file_name() {
        Some(name) => name.to_string_lossy(),
        None => {
            println!("Error getting folder name.");
            return;
        }
    };

    println!("Running build: clang src/main.c -o {}...", folder_name);

    let output = Command::new("clang")
        .arg("src/main.c")
        .arg("-o")
        .arg(folder_name.as_ref())
        .status();

    match output {
        Ok(status) => {
            if status.success() {
                println!("Build completed successfully.");
            } else {
                println!("Build failed with exit code: {:?}", status.code());
            }
        }
        Err(e) => {
            println!("Failed to execute clang. Make sure it is installed.");
            println!("Error: {}", e);
        }
    }
}