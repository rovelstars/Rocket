#![allow(deprecated)]
use bollard::{
    container::{
        Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions,
        WaitContainerOptions,
    },
    secret::{HostConfig, Mount, MountTypeEnum},
};

use std::default::Default;

pub async fn copy_files(
    docker: &bollard::Docker,
    image_id: &str,
) -> Result<(), bollard::errors::Error> {
    let package_name =
        image_id
            .split(':')
            .next()
            .ok_or_else(|| bollard::errors::Error::IOError {
                err: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Invalid image_id: missing package name",
                ),
            })?;
    let container_name = format!("copyfiles-{}", package_name);

    // Define the host config for the container
    let host_config = Some(HostConfig {
        // Specify the mounts here
        mounts: Some(vec![Mount {
            target: Some("/share".to_string()),
            source: Some(
                std::fs::canonicalize("./local_output")?
                    .to_string_lossy()
                    .into_owned(),
            ),
            typ: Some(MountTypeEnum::BIND),
            read_only: Some(false),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let options = Some(CreateContainerOptions {
        name: container_name,
        platform: None,
    });
    
    let cmd_str = format!(
        "cp /output/*.tar.gz /share && [ -d /LICENSES ] && cp -r /LICENSES /share/{}-LICENSES || true",
        package_name
    );

    let config = Config {
        image: Some(image_id),
        cmd: Some(vec!["/bin/sh", "-c", &cmd_str]),
        host_config: host_config,
        ..Default::default()
    };

    let container_id = docker.create_container(options, config).await?.id;

    println!("Created container with ID: {}", container_id);

    let _ = docker
        .start_container(&container_id, None::<StartContainerOptions<String>>)
        .await?;
    println!("Container started: {}", container_id);

    // Use wait_container().next().await to block until the container exits and get its status
    let mut wait_stream =
        docker.wait_container(&container_id, None::<WaitContainerOptions<String>>);
    use futures_util::stream::StreamExt;
    if let Some(result) = wait_stream.next().await {
        println!("Container finished with status: {:?}", result);
    } else {
        println!("Container finished but no status received.");
    }

    let _ = docker
        .remove_container(&container_id, None::<RemoveContainerOptions>)
        .await?;
    println!("Container removed: {}", container_id);

    Ok(())
}
