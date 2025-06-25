use clap::Parser;
mod init;
use init::init_package_container;
mod conn;
use conn::get_docker_connection;

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
    init_package_container(&args.program, docker).await
        .expect("Failed to initialize package container");
}
