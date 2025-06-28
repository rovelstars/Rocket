use std::env;

pub async fn init_package_container(package_name: &str) -> Result<(String, toml::Value, Vec<(String, String)>), String> {
    let packages_dir =
        env::var("PACKAGES_DIR").map_err(|e| format!("PACKAGES_DIR not set: {}", e))?;
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
        format!(
            "{}/{}/BUILD",
            packages_dir.trim_end_matches('/'),
            package_name
        )
    } else {
        // Fallback to relative path
        format!(
            "{}/{}/BUILD",
            packages_dir.trim_end_matches('/'),
            package_name
        )
    };

    //meta.toml is in same directory as BUILD
    let meta_path = format!("{}/meta.toml", build_path.trim_end_matches("/BUILD"));

    let build_content = if build_path.starts_with("https://") {
        // Download from URL
        let resp = reqwest::get(&build_path)
            .await
            .map_err(|e| format!("Failed to fetch BUILD: {}", e))?;
        resp.text()
            .await
            .map_err(|e| format!("Failed to read BUILD content: {}", e))?
    } else {
        std::fs::read_to_string(&build_path).map_err(|e| format!("Failed to read BUILD: {}", e))?
    };
    let meta_content = if meta_path.starts_with("https://") {
        // Download from URL
        let resp = reqwest::get(&meta_path)
            .await
            .map_err(|e| format!("Failed to fetch meta.toml: {}", e))?;
        resp.text()
            .await
            .map_err(|e| format!("Failed to read meta.toml content: {}", e))?
    } else {
        std::fs::read_to_string(&meta_path)
            .map_err(|e| format!("Failed to read meta.toml: {}", e))?
    };

    //read toml contents and replace placeholders in build_content, like {{repository}}
    let meta: toml::Value =
        toml::from_str(&meta_content).map_err(|e| format!("Failed to parse meta.toml: {}", e))?;
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
    //if patches folder exists, copy all the files and store it in array that can be passed back
    let patches_path = format!("{}/patches", build_path.trim_end_matches("/BUILD"));
    let mut patches = Vec::new();
    let collect_patches = |dir: &std::path::Path, patches: &mut Vec<(String, String)>| -> Result<(), String> {
        fn inner(
            dir: &std::path::Path,
            patches: &mut Vec<(String, String)>,
            patches_path: &std::path::Path,
            collect_patches: &dyn Fn(&std::path::Path, &mut Vec<(String, String)>) -> Result<(), String>,
        ) -> Result<(), String> {
            for entry in std::fs::read_dir(dir)
                .map_err(|e| format!("Failed to read patches directory: {}", e))?
            {
                let entry = entry.map_err(|e| format!("Failed to read entry in patches directory: {}", e))?;
                let path = entry.path();
                if path.is_file() {
                    let file_name = path
                        .strip_prefix(patches_path.parent().unwrap_or(patches_path))
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    let content = std::fs::read_to_string(&path)
                        .map_err(|e| format!("Failed to read patch file '{}': {}", file_name, e))?;
                    patches.push((file_name, content));
                } else if path.is_dir() {
                    inner(&path, patches, patches_path, collect_patches)?;
                }
            }
            Ok(())
        }
        let patches_path = std::path::Path::new(&patches_path);
        inner(dir, patches, patches_path, &|d, p| inner(d, p, patches_path, &|d, p| inner(d, p, patches_path, &|_, _| Ok(()))))
    };

    if std::path::Path::new(&patches_path).exists() {
        collect_patches(std::path::Path::new(&patches_path), &mut patches)?;
    }
    Ok((build_content, meta, patches))
}
