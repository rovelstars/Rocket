//return back list of images by name filter, using bollard.

pub async fn list_images(
    docker: &bollard::Docker,
    name: &str,
    version: &str,
) -> Result<String, String> {
    let filters = serde_json::json!({
        "reference": [format!("{}:{}", name, version)],
    });

    let images = docker
        .list_images(
            None,
            Some(bollard::image::ListImagesOptions {
                all: true,
                filters: Some(filters),
            }),
        )
        .await
        .map_err(|e| e.to_string())?;

    if images.is_empty() {
        return Err(format!("No images found for {}:{}", name, version));
    }

    // Return the first image's ID as a string
    Ok(images[0].id.clone())
}