use bollard::Docker;

pub async fn init_package_container(package_name: &str, _docker: Docker) -> Result<(), String> {
    //make docker container from the ./packages/<package_name>.Dockerfile, and also read this file's labels so we know where's the output
    let dockerfile_path = format!("./packages/{}.Dockerfile", package_name);
    let dockerfile_content = std::fs::read_to_string(&dockerfile_path)
        .map_err(|e| format!("Failed to read Dockerfile: {}", e))?;
    println!("Using Dockerfile at: {}", dockerfile_path);
    println!("Content:\n{}", dockerfile_content);
    Ok(())
}
