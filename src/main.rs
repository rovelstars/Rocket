mod config;
mod sandbox;
mod builder;

use clap::Parser;
use colored::Colorize;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "rocket", about = "RunixOS package builder")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Build a package from Planets
    Build {
        /// Package name (directory in packages/)
        package: String,
        /// Path to Planets repository
        #[arg(short, long, default_value = "../Planets")]
        planets: PathBuf,
        /// Output directory for built artifacts
        #[arg(short, long, default_value = "./output")]
        output: PathBuf,
        /// Path to RunixOS sysroot (build environment)
        #[arg(short, long, default_value = "/home/ren/ROS")]
        sysroot: PathBuf,
        /// Build a local working tree instead of cloning upstream
        /// (overrides meta.toml `local_path`). Exposed to build.sh as $LOCAL_SRC.
        #[arg(short, long)]
        local: Option<PathBuf>,
    },
    /// Build all packages
    BuildAll {
        #[arg(short, long, default_value = "../Planets")]
        planets: PathBuf,
        #[arg(short, long, default_value = "./output")]
        output: PathBuf,
        #[arg(short, long, default_value = "/home/ren/ROS")]
        sysroot: PathBuf,
    },
    /// List available packages
    List {
        #[arg(short, long, default_value = "../Planets")]
        planets: PathBuf,
    },
    /// Enter the RunixOS sandbox interactively
    Enter {
        #[arg(short, long, default_value = "/home/ren/ROS")]
        sysroot: PathBuf,
        /// Mount host tools (/usr/bin, /usr/lib) into the sandbox
        #[arg(long)]
        enable_host_links: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let is_root = nix::unistd::geteuid().is_root();

    if !is_root {
        eprintln!("{}", "TIP: Running with sudo enables chroot mode which is faster for large builds.".yellow());
        eprintln!("{}", "     Use: sudo rocket build <package>".yellow());
        eprintln!();
    }

    match cli.command {
        Command::Build { package, planets, output, sysroot, local } => {
            let pkg_dir = planets.join("packages").join(&package);
            if !pkg_dir.exists() {
                eprintln!("{} Package '{}' not found at {:?}", "Error:".red().bold(), package, pkg_dir);
                std::process::exit(1);
            }
            match config::load_package(&pkg_dir) {
                Ok(pkg) => {
                    println!("{} {} v{}", "Building".green().bold(), pkg.meta.name, pkg.meta.version);
                    if let Err(e) = builder::build_package(&pkg, &sysroot, &output, is_root, local.as_deref()) {
                        eprintln!("{} {}", "Build failed:".red().bold(), e);
                        std::process::exit(1);
                    }
                    println!("{} {} v{}", "Completed".green().bold(), pkg.meta.name, pkg.meta.version);
                }
                Err(e) => {
                    eprintln!("{} Failed to load package: {}", "Error:".red().bold(), e);
                    std::process::exit(1);
                }
            }
        }
        Command::BuildAll { planets, output, sysroot } => {
            let pkgs_dir = planets.join("packages");
            let mut packages: Vec<String> = std::fs::read_dir(&pkgs_dir)
                .expect("Cannot read packages directory")
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            packages.sort();
            println!("{} {} packages", "Building".green().bold(), packages.len());
            for name in &packages {
                let pkg_dir = pkgs_dir.join(name);
                match config::load_package(&pkg_dir) {
                    Ok(pkg) => {
                        println!("\n{} {} v{}", "Building".green().bold(), pkg.meta.name, pkg.meta.version);
                        if let Err(e) = builder::build_package(&pkg, &sysroot, &output, is_root, None) {
                            eprintln!("{} {}: {}", "Failed:".red().bold(), name, e);
                        }
                    }
                    Err(e) => eprintln!("{} {}: {}", "Skip:".yellow(), name, e),
                }
            }
        }
        Command::List { planets } => {
            let pkgs_dir = planets.join("packages");
            let mut entries: Vec<_> = std::fs::read_dir(&pkgs_dir)
                .expect("Cannot read packages directory")
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let pkg_dir = entry.path();
                if let Ok(pkg) = config::load_package(&pkg_dir) {
                    println!("  {} {} - {}",
                        pkg.meta.name.green(),
                        format!("v{}", pkg.meta.version).dimmed(),
                        pkg.meta.description);
                }
            }
        }
        Command::Enter { sysroot, enable_host_links } => {
            println!("{} RunixOS sandbox at {:?}{}", "Entering".cyan().bold(), sysroot,
                if enable_host_links { " (host links enabled)" } else { "" });
            if let Err(e) = sandbox::enter_interactive(&sysroot, is_root, enable_host_links) {
                eprintln!("{} {}", "Error:".red().bold(), e);
                std::process::exit(1);
            }
        }
    }
}
