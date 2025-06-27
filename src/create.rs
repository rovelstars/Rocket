#![allow(deprecated)]
use std::io::Write;

use bollard::image::BuildImageOptions;
use futures_util::TryStreamExt;
use http_body_util::Full;

pub async fn create_container(
    docker: &bollard::Docker,
    build_script: &str,
    meta: &toml::Value,
) -> Result<String, String> {
    println!("[create_container] build_script: {}", build_script);

    // Create a container with the build script and metadata
    let name = meta
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Missing 'name' in metadata".to_string())?;
    let version = meta
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("latest");
    let container_name = format!("{}-{}", name, version);

    //create a tar archive of the build script, so it can be used to create a container

    let mut header = tar::Header::new_gnu();
    header.set_path("Dockerfile").unwrap();
    header.set_size(build_script.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let mut tar = tar::Builder::new(Vec::new());
    tar.append(&header, build_script.as_bytes()).unwrap();

    let uncompressed = tar.into_inner().unwrap();
    let mut c = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    c.write_all(&uncompressed).unwrap();
    let compressed = c.finish().unwrap();

    let result = &docker
        .build_image(
            BuildImageOptions {
                dockerfile: "Dockerfile".to_string(),
                t: container_name.to_string(),
                pull: true,
                rm: true,
                ..Default::default()
            },
            //if cfg!(windows) { None } else { Some(creds) },
            None,
            Some(http_body_util::Either::Left(Full::new(compressed.into()))),
        )
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| e.to_string())?;

    // If all went well, the ID of the new image will be printed
    println!(
        "[create_container] Created image: {:?}\n{}-{}",
        &result[0], name, version
    );

    Ok(format!(
        "{}-{}",
        meta.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown"),
        meta.get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("latest")
    ))
}
