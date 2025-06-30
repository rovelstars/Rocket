#![allow(deprecated)]
//return back list of images by name filter, using bollard.

pub async fn list_images(
    docker: &bollard::Docker,
    name: &str,
    version: &str,
) -> Result<String, String> {
    use bollard::image::ListImagesOptions;

    let options = Some(ListImagesOptions {
        all: true,
        filters: {
            let mut map = std::collections::HashMap::new();
            map.insert("reference".to_string(), vec![format!("{}:{}", name, version)]);
            map
        },
        ..Default::default()
    });

    match docker.list_images(options).await {
        Ok(images) => {
            let image_list: Vec<String> = images
                .into_iter()
                .map(|img| img.repo_tags.join(", "))
                .collect();
            Ok(image_list.join("\n"))
        }
        Err(e) => Err(format!("Failed to list images: {}", e)),
    }
}