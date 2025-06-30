use clap::Parser;
use std::env;
mod conn;
mod copy;
mod create;
mod init;
mod list_images;
use colored::*;
use conn::get_docker_connection;
use copy::copy_files;
use create::create_container;
use init::init_package_container;
use list_images::list_images;
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    // The program to build
    #[arg(short, long, default_value = "<ALL>")]
    program: String,
    #[arg(short, long, default_value = "false")]
    dry_run: bool,
    #[arg(long, default_value = "false")]
    skip_copy_files: bool,
    #[arg(long, default_value = "false")]
    skip_image_build: bool,
    #[arg(short,long, default_value = "false")]
    force: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let docker = get_docker_connection()
        .await
        .expect("Failed to connect to Docker");

    if args.program == "<ALL>" {
        println!("Building all programs...");
        let packages_dir =
            env::var("PACKAGES_DIR").map_err(|e| format!("PACKAGES_DIR not set: {}", e));
        let packages_dir = match packages_dir {
            Ok(dir) => dir,
            Err(e) => {
                println!("{}", e);
                return;
            }
        };
        if packages_dir.starts_with("https://") || packages_dir.starts_with("//") {
            println!(
                "{}",
                "Currently only local packages are supported for multiple package installation."
                    .red()
            );
        } else if packages_dir.starts_with("file://") {
            println!("{}","Expected folder path, not file:// URL. Please set PACKAGES_DIR to a local folder path when building all programs.".red());
        } else {
            let packages = std::fs::read_dir(packages_dir)
                .expect("Failed to read packages directory")
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().into_string().unwrap())
                .collect::<Vec<_>>();

            for package in packages {
                println!("Building package: {}", package);
                if let Err(e) = create(
                    &docker,
                    &package,
                    args.dry_run,
                    args.skip_copy_files,
                    args.skip_image_build,
                )
                .await
                {
                    eprintln!("Error building package {}: {}", package, e);
                }
            }
            println!("All Planets Created.");
        }
    } else {
        println!("Building program: {}", args.program);
        if let Err(e) = create(
            &docker,
            &args.program,
            args.dry_run,
            args.skip_copy_files,
            args.skip_image_build,
        )
        .await
        {
            eprintln!("Error building program {}: {}", args.program, e);
        }
        println!("Planet Created.");
    }
}

async fn create(
    docker: &bollard::Docker,
    program: &str,
    dry_run: bool,
    dont_copy: bool,
    dont_build_image: bool,
) -> Result<(), String> {
    let (build_script, meta, patches) = init_package_container(&program)
        .await
        .expect("Failed to initialize package container");

    let package_name = meta
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let version = meta
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("latest");

    println!("Package name: {}", package_name);
    println!("Version: {}", version);

    let list_of_images = list_images(&docker, package_name, version)
        .await
        .expect("Failed to list images");
    println!("List of images:\n{}", list_of_images);

    create_container(
        &docker,
        &build_script,
        &meta,
        &patches,
        &(dry_run || dont_build_image),
    )
    .await
    .expect("Failed to create container");
    if !dont_copy {
        println!("Copying files...");
        if dry_run {
            println!("Dry run is enabled, skipping file copy.");
        } else {
            copy_files(&docker, &format!("{}:{}", package_name, version))
                .await
                .expect("Failed to copy files");
        }
    } else {
        println!("Skipping output copy.");
    }
    Ok(())
}
