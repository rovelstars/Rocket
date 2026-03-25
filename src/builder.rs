use crate::config::Package;
use crate::sandbox;
use std::path::Path;

/// Build a package inside the sandbox
pub fn build_package(
    pkg: &Package,
    sysroot: &Path,
    output: &Path,
    is_root: bool,
) -> Result<(), String> {
    // Create output directory
    std::fs::create_dir_all(output)
        .map_err(|e| format!("mkdir output: {}", e))?;

    // Create source directory in sysroot
    let src_dir = sysroot.join("Transit/Ephemeral/build");
    std::fs::create_dir_all(&src_dir)
        .map_err(|e| format!("mkdir build dir: {}", e))?;

    // Copy build.sh into the build directory
    let build_sh_dest = src_dir.join("build.sh");
    std::fs::copy(&pkg.build_script, &build_sh_dest)
        .map_err(|e| format!("copy build.sh: {}", e))?;

    // Copy patches if present
    if let Some(patches) = &pkg.patches_dir {
        let patches_dest = src_dir.join("patches");
        copy_dir_recursive(patches, &patches_dest)?;
    }

    // For RunixOS cross-compilation, packages need our patched libc crate.
    // We inject [patch.crates-io] into each project's Cargo.toml at build time.
    // This is done in build.sh via the RUNIXOS_LIBC_PATH env var.

    // Paths: in root mode, use absolute /Transit/... (chroot'd)
    // In non-root mode, use sysroot-prefixed paths
    let (src_path, out_path, patches_path) = if is_root {
        (
            "/Transit/Ephemeral/build".to_string(),
            "/Transit/Ephemeral/output".to_string(),
            "/Transit/Ephemeral/build/patches".to_string(),
        )
    } else {
        (
            src_dir.to_string_lossy().to_string(),
            sysroot.join("Transit/Ephemeral/output").to_string_lossy().to_string(),
            src_dir.join("patches").to_string_lossy().to_string(),
        )
    };

    // Build environment variables from meta.toml
    let mut envs: Vec<(String, String)> = vec![
        ("NAME".into(), pkg.meta.name.clone()),
        ("VERSION".into(), pkg.meta.version.clone()),
        ("REPOSITORY".into(), pkg.meta.repository.clone()),
        ("OUTPUT".into(), out_path.clone()),
        ("SRC".into(), src_path.clone()),
        ("PATCHES".into(), patches_path.clone()),
        ("SYSROOT".into(), sysroot.to_string_lossy().to_string()),
        ("JOBS".into(), num_cpus().to_string()),
        ("ROCKET_OUTPUT".into(), output.parent().unwrap_or(output).to_string_lossy().to_string()),
    ];

    // Add extra fields from meta.toml as env vars
    for (key, value) in &pkg.meta.extra {
        let val = match value {
            toml::Value::String(s) => s.clone(),
            toml::Value::Boolean(b) => b.to_string(),
            toml::Value::Integer(i) => i.to_string(),
            other => other.to_string(),
        };
        envs.push((key.to_uppercase(), val));
    }

    let env_refs: Vec<(&str, &str)> = envs.iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // Run build.sh inside sandbox
    let build_cmd = format!(
        "cd {src} && \
         mkdir -p {out} && \
         source ./build.sh && \
         if type configure >/dev/null 2>&1; then echo '>>> configure' && configure; fi && \
         if type build >/dev/null 2>&1; then echo '>>> build' && build; fi && \
         if type install >/dev/null 2>&1; then echo '>>> install' && install; fi",
        src = src_path, out = out_path
    );

    let code = sandbox::run_in_sandbox(
        sysroot,
        &["/host/bin/sh", "-e", "-c", &build_cmd],
        &env_refs,
        is_root,
        true, // builds always need host tools (cargo, rustc, etc.)
    )?;

    if code != 0 {
        return Err(format!("Build script exited with code {}", code));
    }

    // Copy output artifacts from sysroot to host output directory
    let sandbox_output = sysroot.join("Transit/Ephemeral/output");
    if sandbox_output.exists() {
        let pkg_output = output.join(&pkg.meta.name);
        std::fs::create_dir_all(&pkg_output)
            .map_err(|e| format!("mkdir pkg output: {}", e))?;
        copy_dir_recursive(&sandbox_output, &pkg_output)?;
        println!("  Output: {:?}", pkg_output);
    }

    // Clean up build directory
    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&sandbox_output);

    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("mkdir {:?}: {}", dst, e))?;
    for entry in std::fs::read_dir(src)
        .map_err(|e| format!("readdir {:?}: {}", src, e))? {
        let entry = entry.map_err(|e| format!("entry: {}", e))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type().map_err(|e| format!("filetype: {}", e))?;
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_symlink() {
            // Preserve symlinks
            let target = std::fs::read_link(&src_path)
                .map_err(|e| format!("readlink {:?}: {}", src_path, e))?;
            let _ = std::fs::remove_file(&dst_path);
            std::os::unix::fs::symlink(&target, &dst_path)
                .map_err(|e| format!("symlink {:?} -> {:?}: {}", dst_path, target, e))?;
        } else {
            std::fs::copy(&src_path, &dst_path)
                .map_err(|e| format!("copy {:?}: {}", src_path, e))?;
        }
    }
    Ok(())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
