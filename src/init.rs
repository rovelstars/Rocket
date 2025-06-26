use bollard::Docker;
use std::env;

pub async fn init_package_container(package_name: &str, _docker: Docker) -> Result<(), String> {
    let packages_dir = env::var("PACKAGES_DIR").map_err(|e| format!("PACKAGES_DIR not set: {}", e))?;
    let build_path = if packages_dir.starts_with("file://") {
        format!("{}/{}/BUILD", &packages_dir[7..], package_name)
    } else if packages_dir.starts_with("https://") || packages_dir.starts_with("//") {
        // Online file
        let base = if packages_dir.starts_with("//") {
            format!("https:{}", packages_dir)
        } else {
            packages_dir.clone()
        };
        format!("{}/{}/BUILD", base.trim_end_matches('/'), package_name)
    } else if packages_dir.starts_with('/') {
        // Local absolute path
        format!("{}/{}/BUILD", packages_dir.trim_end_matches('/'), package_name)
    } else {
        // Fallback to relative path
        format!("{}/{}/BUILD", packages_dir.trim_end_matches('/'), package_name)
    };
    println!("Using BUILD path: {}", build_path);
    
    //meta.toml is in same directory as BUILD
    let meta_path = format!("{}/meta.toml", build_path.trim_end_matches("/BUILD"));
    println!("Using meta.toml at: {}", meta_path);


    let build_content = if build_path.starts_with("https://") {
        // Download from URL
        let resp = reqwest::get(&build_path).await.map_err(|e| format!("Failed to fetch BUILD: {}", e))?;
        resp.text().await.map_err(|e| format!("Failed to read BUILD content: {}", e))?
    } else {
        std::fs::read_to_string(&build_path)
            .map_err(|e| format!("Failed to read BUILD: {}", e))?
    };
    let meta_content = if meta_path.starts_with("https://") {
        // Download from URL
        let resp = reqwest::get(&meta_path).await.map_err(|e| format!("Failed to fetch meta.toml: {}", e))?;
        resp.text().await.map_err(|e| format!("Failed to read meta.toml content: {}", e))?
    } else {
        std::fs::read_to_string(&meta_path)
            .map_err(|e| format!("Failed to read meta.toml: {}", e))?
    };

    //read toml contents and replace placeholders in build_content, like {{repository}}
    let meta: toml::Value = toml::from_str(&meta_content).map_err(|e| format!("Failed to parse meta.toml: {}", e))?;
    // Replace all placeholders in build_content with values from meta
    let mut replaced_content = build_content.clone();
    if let Some(table) = meta.as_table() {
        for (key, value) in table {
            if let Some(val_str) = value.as_str() {
                let placeholder = format!("{{{{{}}}}}", key);
                replaced_content = replaced_content.replace(&placeholder, val_str);
            }
        }
    }
    let build_content = replaced_content;
    println!("Parsed BUILD content:\n{}", build_content);
    Ok(())
}
