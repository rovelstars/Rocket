mod config;
mod sandbox;
mod builder;
mod resolver;

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
        /// Build this package's dependencies first, in order.
        #[arg(long)]
        with_deps: bool,
        /// Install the built package into the sysroot so later builds can use it.
        /// (Always on with --with-deps and for build-all.)
        #[arg(long)]
        install: bool,
        /// Rebuild even if the build cache says it is unchanged.
        #[arg(long)]
        force: bool,
    },
    /// Build all packages
    BuildAll {
        #[arg(short, long, default_value = "../Planets")]
        planets: PathBuf,
        #[arg(short, long, default_value = "./output")]
        output: PathBuf,
        #[arg(short, long, default_value = "/home/ren/ROS")]
        sysroot: PathBuf,
        /// Do not install each package into the sysroot between builds.
        /// (Inter-package dependencies will not resolve; for debugging only.)
        #[arg(long)]
        no_install: bool,
        /// Rebuild every package, ignoring the build cache.
        #[arg(long)]
        force: bool,
    },
    /// List available packages
    List {
        #[arg(short, long, default_value = "../Planets")]
        planets: PathBuf,
    },
    /// Print the resolved dependency build order (no building).
    Deps {
        /// Optional package(s); prints their dependency closure. Omit for all.
        packages: Vec<String>,
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

fn load_all_or_exit(planets: &std::path::Path) -> (Vec<config::Package>, Vec<String>) {
    match config::load_all(&planets.join("packages")) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} {}", "Error:".red().bold(), e);
            std::process::exit(1);
        }
    }
}

/// Build packages in the given order, fail-fast with a summary. `local_for`
/// supplies an optional local-source override per package name.
fn build_in_order(
    pkgs: &[config::Package],
    order: &[String],
    sysroot: &std::path::Path,
    output: &std::path::Path,
    install: bool,
    force: bool,
    local_for: impl Fn(&str) -> Option<PathBuf>,
) {
    use std::collections::HashMap;
    let by_name: HashMap<&str, &config::Package> =
        pkgs.iter().map(|p| (p.meta.name.as_str(), p)).collect();
    let mut built = 0usize;
    for name in order {
        let Some(pkg) = by_name.get(name.as_str()) else {
            eprintln!("{} {} (no package dir)", "Failed:".red().bold(), name);
            std::process::exit(1);
        };
        println!("\n{} {} v{}", "Building".green().bold(), pkg.meta.name, pkg.meta.version);
        let loc = local_for(name);
        if let Err(e) = builder::build_package(pkg, sysroot, output, loc.as_deref(), install, force) {
            eprintln!("{} {}: {}", "Failed:".red().bold(), name, e);
            eprintln!(
                "{} built {}/{} before stopping at {}",
                "Summary:".yellow(),
                built,
                order.len(),
                name
            );
            std::process::exit(1);
        }
        built += 1;
    }
    println!("\n{} built {}/{} packages", "Done:".green().bold(), built, order.len());
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Build { package, planets, output, sysroot, local, with_deps, install, force } => {
            if with_deps {
                // Build the dependency closure in order; the target builds last.
                let (pkgs, errors) = load_all_or_exit(&planets);
                for e in &errors {
                    eprintln!("{} {}", "Skip:".yellow(), e);
                }
                let order = match resolver::resolve_order(&pkgs, Some(&[package.clone()])) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("{} {}", "Dependency error:".red().bold(), e);
                        std::process::exit(1);
                    }
                };
                println!("{} {} (closure of {})", "Build order:".cyan().bold(), order.join(" -> "), package);
                // Dependencies must be installed into the sysroot so the target
                // can build against them, so a closure build always installs.
                // local override only applies to the named target, not its deps.
                build_in_order(&pkgs, &order, &sysroot, &output, true, force, |n| {
                    if n == package { local.clone() } else { None }
                });
            } else {
                let pkg_dir = planets.join("packages").join(&package);
                if !pkg_dir.exists() {
                    eprintln!("{} Package '{}' not found at {:?}", "Error:".red().bold(), package, pkg_dir);
                    std::process::exit(1);
                }
                match config::load_package(&pkg_dir) {
                    Ok(pkg) => {
                        println!("{} {} v{}", "Building".green().bold(), pkg.meta.name, pkg.meta.version);
                        if let Err(e) = builder::build_package(&pkg, &sysroot, &output, local.as_deref(), install, force) {
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
        }
        Command::BuildAll { planets, output, sysroot, no_install, force } => {
            let (pkgs, errors) = load_all_or_exit(&planets);
            for e in &errors {
                eprintln!("{} {}", "Skip:".yellow(), e);
            }
            let order = match resolver::resolve_order(&pkgs, None) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("{} {}", "Dependency error:".red().bold(), e);
                    std::process::exit(1);
                }
            };
            println!("{} {} packages in dependency order", "Building".green().bold(), order.len());
            build_in_order(&pkgs, &order, &sysroot, &output, !no_install, force, |_| None);
        }
        Command::Deps { packages, planets } => {
            let (pkgs, errors) = load_all_or_exit(&planets);
            for e in &errors {
                eprintln!("{} {}", "Skip:".yellow(), e);
            }
            let targets = if packages.is_empty() { None } else { Some(packages.as_slice()) };
            match resolver::resolve_order(&pkgs, targets) {
                Ok(order) => {
                    for (i, name) in order.iter().enumerate() {
                        println!("  {:>2}. {}", i + 1, name.green());
                    }
                }
                Err(e) => {
                    eprintln!("{} {}", "Dependency error:".red().bold(), e);
                    std::process::exit(1);
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
            if let Err(e) = sandbox::enter_interactive(&sysroot, enable_host_links) {
                eprintln!("{} {}", "Error:".red().bold(), e);
                std::process::exit(1);
            }
        }
    }
}
