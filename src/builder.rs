use crate::config::Package;
use crate::sandbox;
use std::path::Path;

/// Build a package inside the sandbox.
///
/// `local_src`, when set, points at a local working tree (e.g. ../llvm-project)
/// that the build script clones-or-symlinks instead of fetching upstream. It is
/// resolved from the CLI `--local` flag, falling back to the package's
/// `local_path` field in meta.toml.
pub fn build_package(
    pkg: &Package,
    sysroot: &Path,
    output: &Path,
    is_root: bool,
    local_src: Option<&Path>,
    install_to_sysroot: bool,
) -> Result<(), String> {
    // Create output directory
    std::fs::create_dir_all(output)
        .map_err(|e| format!("mkdir output: {}", e))?;

    // Persistent per-package build directory on real disk. NOT under
    // Transit/Ephemeral - that gets a tmpfs mount in root mode which would
    // shadow the staged files and discard build output. Transit/Build is a
    // plain directory so it survives the sandbox and enables incremental
    // rebuilds (critical for llvm/glibc - full rebuilds take hours).
    let src_dir = sysroot.join("Transit/Build").join(&pkg.meta.name);
    std::fs::create_dir_all(&src_dir)
        .map_err(|e| format!("mkdir build dir: {}", e))?;

    // Fresh output dir each run so stale installs don't leak into artifacts.
    let out_dir = src_dir.join("_out");
    let _ = std::fs::remove_dir_all(&out_dir);

    // Resolve local source: CLI override first, then meta.toml local_path
    // (resolved relative to the package directory).
    let local_resolved: Option<std::path::PathBuf> = match local_src {
        Some(p) => Some(p.to_path_buf()),
        None => pkg.meta.extra.get("local_path").and_then(|v| v.as_str())
            .map(|s| pkg.pkg_dir.join(s)),
    };
    let local_canon = match local_resolved {
        Some(p) => Some(p.canonicalize()
            .map_err(|e| format!("local source {:?}: {}", p, e))?),
        None => None,
    };

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

    // Paths: in root mode, use absolute /Transit/Build/<pkg> (chroot'd).
    // In non-root mode, use sysroot-prefixed host paths.
    let pkg_build = format!("/Transit/Build/{}", pkg.meta.name);
    let (src_path, out_path, patches_path) = if is_root {
        (
            pkg_build.clone(),
            format!("{}/_out", pkg_build),
            format!("{}/patches", pkg_build),
        )
    } else {
        (
            src_dir.to_string_lossy().to_string(),
            out_dir.to_string_lossy().to_string(),
            src_dir.join("patches").to_string_lossy().to_string(),
        )
    };

    // Local-source plumbing. In root mode the chroot can't see host paths, so
    // bind-mount the local tree to <build>/src and point $LOCAL_SRC at it. In
    // non-root mode $LOCAL_SRC is just the canonical host path.
    let mut binds: Vec<(std::path::PathBuf, String)> = Vec::new();
    let local_src_env: Option<String> = local_canon.as_ref().map(|lc| {
        if is_root {
            binds.push((lc.clone(), format!("Transit/Build/{}/src", pkg.meta.name)));
            format!("{}/src", pkg_build)
        } else {
            lc.to_string_lossy().to_string()
        }
    });
    if let Some(ref ls) = local_src_env {
        println!("  Local source: {}", ls);
    }

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
    if let Some(ls) = &local_src_env {
        envs.push(("LOCAL_SRC".into(), ls.clone()));
    }

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
        false, // non-interactive: fork so we regain control to copy artifacts
        &binds,
    )?;

    if code != 0 {
        return Err(format!("Build script exited with code {}", code));
    }

    // Copy output artifacts to host output directory. out_dir is on real disk
    // (Transit/Build/<pkg>/_out), so it survives even root-mode sandboxes.
    if out_dir.exists() {
        let pkg_output = output.join(&pkg.meta.name);
        let _ = std::fs::remove_dir_all(&pkg_output);
        std::fs::create_dir_all(&pkg_output)
            .map_err(|e| format!("mkdir pkg output: {}", e))?;
        copy_dir_recursive(&out_dir, &pkg_output)?;
        println!("  Output: {:?}", pkg_output);

        // Merge the output into the sysroot so packages built later in a
        // dependency-ordered run can find this package's headers and libraries
        // (e.g. curl needs openssl in Core/LibKit + Core/APIHeader). Without
        // this, every package builds against an unchanging sysroot and inter
        // package dependencies never resolve.
        if install_to_sysroot {
            copy_dir_recursive(&out_dir, sysroot)?;
            println!("  Installed into sysroot: {:?}", sysroot);
        }
    } else if install_to_sysroot {
        return Err(format!(
            "nothing to install: {} produced no output at {:?}",
            pkg.meta.name, out_dir
        ));
    }

    // Build directory is intentionally kept for incremental rebuilds.
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
