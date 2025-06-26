use clap::Parser;

mod init;
mod conn;
mod list;
mod create;

use init::init_package_container;
use conn::get_docker_connection;
use list::list_package_containers;
use create::create_container;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    // The program to build
    #[arg(short, long, default_value = "hello_world")]
    program: String,
}
#[tokio::main]
async fn main() {
    let args = Args::parse();
    println!("Building program: {}", args.program);
    let docker = get_docker_connection()
        .await
        .expect("Failed to connect to Docker");
    // Here you would call your build function, e.g., build_program(&args.program);
    // For now, we just print the program name.
    let (build_script, meta) = 
        init_package_container(&args.program).await
        .expect("Failed to initialize package container");
    let package_name = meta.get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let version = meta.get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("latest");
    println!("Build script: {}", build_script);
    println!("Package name: {}", package_name);
    println!("Version: {}", version);
    let list_containers = list_package_containers(
        &docker,
        package_name,
    );
    match list_containers.await {
        Ok(containers) => {
            println!("Containers for package '{}':", package_name);
            for container in containers {
                println!("- {}", container);
            }
        }
        Err(e) => {
            eprintln!("Error listing containers: {}", e);
        }
    }
    create_container(&docker, &build_script, &meta)
        .await
        .expect("Failed to create container");
}
