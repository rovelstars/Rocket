use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct PackageMeta {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub licenses: Vec<String>,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Extra key-value pairs for build.sh environment
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

pub struct Package {
    pub meta: PackageMeta,
    pub build_script: PathBuf,
    pub patches_dir: Option<PathBuf>,
    pub pkg_dir: PathBuf,
}

pub fn load_package(pkg_dir: &Path) -> Result<Package, String> {
    let meta_path = pkg_dir.join("meta.toml");
    if !meta_path.exists() {
        return Err(format!("No meta.toml in {:?}", pkg_dir));
    }

    let meta_str = std::fs::read_to_string(&meta_path)
        .map_err(|e| format!("Failed to read meta.toml: {}", e))?;
    let meta: PackageMeta = toml::from_str(&meta_str)
        .map_err(|e| format!("Failed to parse meta.toml: {}", e))?;

    let build_script = pkg_dir.join("build.sh");
    if !build_script.exists() {
        return Err(format!("No build.sh in {:?}", pkg_dir));
    }

    let patches_dir = pkg_dir.join("patches");
    let patches_dir = if patches_dir.exists() { Some(patches_dir) } else { None };

    Ok(Package {
        meta,
        build_script,
        patches_dir,
        pkg_dir: pkg_dir.to_path_buf(),
    })
}
