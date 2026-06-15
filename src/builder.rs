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
    local_src: Option<&Path>,
    install_to_sysroot: bool,
    force: bool,
    self_hosted: bool,
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

    // Build cache. The package's build is keyed by its build.sh + meta.toml and,
    // for a local-source package, the git state of that tree. If that matches the
    // last successful install and --force was not given, skip: the package is
    // already in the sysroot. Only applies when installing (a non-install build
    // leaves nothing in the sysroot to reuse).
    let stamp = src_dir.join(".rocket-stamp");
    let key = build_key(pkg, local_canon.as_deref());
    if install_to_sysroot
        && !force
        && std::fs::read_to_string(&stamp).map(|p| p.trim() == key).unwrap_or(false)
    {
        println!("  Skipped (cached, unchanged; --force to rebuild)");
        return Ok(());
    }

    // Fresh output dir each run so stale installs don't leak into artifacts.
    let out_dir = src_dir.join("_out");
    let _ = std::fs::remove_dir_all(&out_dir);

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

    // The build runs chroot'd to the sysroot, so all build paths are absolute
    // inside the chroot. The host-side equivalents (src_dir/out_dir) are only
    // used by this process to stage build.sh and to copy artifacts out.
    let pkg_build = format!("/Transit/Build/{}", pkg.meta.name);
    let src_path = pkg_build.clone();
    let out_path = format!("{}/_out", pkg_build);
    let patches_path = format!("{}/patches", pkg_build);

    // Local-source plumbing: bind the host tree to <build>/src inside the chroot
    // and point $LOCAL_SRC at it (the chroot cannot see arbitrary host paths).
    let mut binds: Vec<(std::path::PathBuf, String)> = Vec::new();
    let local_src_env: Option<String> = local_canon.as_ref().map(|lc| {
        binds.push((lc.clone(), format!("Transit/Build/{}/src", pkg.meta.name)));
        format!("{}/src", pkg_build)
    });
    if let Some(ref ls) = local_src_env {
        println!("  Local source: {}", ls);
    }

    // Sibling source repos a workspace package path-depends on (e.g. RevKit
    // path-deps ../RookGuard, ../UAC, ../WireBus). meta.toml `sibling_paths`
    // lists them relative to the package dir; each host tree is bound next to the
    // package's src (Transit/Build/<pkg>/<basename>) so its `../<repo>` Cargo
    // path deps resolve inside the chroot.
    if let Some(toml::Value::Array(sibs)) = pkg.meta.extra.get("sibling_paths") {
        for s in sibs {
            let Some(rel) = s.as_str() else { continue };
            let host = pkg.pkg_dir.join(rel);
            match host.canonicalize() {
                Ok(canon) => {
                    let base = canon.file_name().unwrap_or_default().to_string_lossy().to_string();
                    binds.push((canon, format!("Transit/Build/{}/{}", pkg.meta.name, base)));
                    println!("  Sibling: {}", base);
                }
                Err(_) => eprintln!("  warning: sibling_paths {:?} not found; skipping", rel),
            }
        }
    }

    // Build environment variables from meta.toml. SYSROOT is "/" because the
    // build is chroot'd into the sysroot, so $SYSROOT/Core/Bin/clang etc resolve.
    let mut envs: Vec<(String, String)> = vec![
        ("NAME".into(), pkg.meta.name.clone()),
        ("VERSION".into(), pkg.meta.version.clone()),
        ("REPOSITORY".into(), pkg.meta.repository.clone()),
        ("OUTPUT".into(), out_path.clone()),
        ("SRC".into(), src_path.clone()),
        ("PATCHES".into(), patches_path.clone()),
        ("SYSROOT".into(), "/".into()),
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

    // Self-hosted: build with ONLY the sysroot-native tools (no host /usr, /bin
    // binds). This proves the RunixOS build environment is self-contained.
    // Default: bind host tools to drive the cross build.
    //
    // make and configure scripts hardcode /bin/sh, which a no-host-links sysroot
    // lacks, so ensure /bin/sh -> the native bash (/Core/Bin/sh). bash is used
    // rather than brush as the build shell: configure/cmake-generated scripts are
    // far more bash-tested.
    // Two shells, kept distinct: the OUTER shell sources build.sh + runs its
    // functions (bash - build.sh uses `source` and may use bash features), while
    // /bin/sh is what configure runs its pipelines under (dash). dash is the lean
    // POSIX sh autotools is tested against; our bash retains pipeline pipe fds and
    // deadlocks `clang -E | grep` on >64KB output, brush is less complete - dash
    // sidesteps both (brush stays the OS shell).
    let (shell, host_links): (&str, bool) = if self_hosted {
        let bin = sysroot.join("bin");
        let _ = std::fs::create_dir_all(&bin);
        let sh = bin.join("sh");
        let _ = std::fs::remove_file(&sh);
        let target = if sysroot.join("Core/Bin/dash").exists() {
            "/Core/Bin/dash"
        } else {
            "/Core/Bin/sh"
        };
        let _ = std::os::unix::fs::symlink(target, &sh);
        ("/Core/Bin/bash", false)
    } else {
        ("/bin/sh", true)
    };
    let code = sandbox::run_in_sandbox(
        sysroot,
        &[shell, "-e", "-c", &build_cmd],
        &env_refs,
        host_links,
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

        // Provenance: emit a package manifest (files + hashes + ELF needs) next
        // to the output, so RuneForge composes by package set + dependency
        // closure instead of merging blind trees.
        emit_package_manifest(&pkg.meta, &pkg_output)?;

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

    // Record the successful install so an unchanged rebuild can be skipped.
    if install_to_sysroot {
        let _ = std::fs::write(&stamp, &key);
    }

    // Build directory is intentionally kept for incremental rebuilds.
    Ok(())
}

/// Content key for the build cache: the build script + meta.toml, plus the git
/// state of the local source tree (HEAD + uncommitted diff + untracked list) so
/// that editing a ported tree (llvm-project, glibc, ...) invalidates the cache.
fn build_key(pkg: &Package, local: Option<&Path>) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::fs::read(&pkg.build_script).unwrap_or_default().hash(&mut h);
    std::fs::read(pkg.pkg_dir.join("meta.toml")).unwrap_or_default().hash(&mut h);
    if let Some(l) = local {
        let l = l.to_string_lossy().to_string();
        let runs: [&[&str]; 3] = [
            &["rev-parse", "HEAD"],
            &["diff", "HEAD"],
            &["status", "--porcelain"],
        ];
        for r in runs {
            if let Ok(o) = std::process::Command::new("git").arg("-C").arg(&l).args(r).output() {
                o.stdout.hash(&mut h);
            }
        }
    }
    format!("{:016x}", h.finish())
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
            // Remove any existing dst first: a read-only file (e.g. a 0444 CA
            // bundle) cannot be overwritten in place by fs::copy.
            let _ = std::fs::remove_file(&dst_path);
            std::fs::copy(&src_path, &dst_path)
                .map_err(|e| format!("copy {:?}: {}", src_path, e))?;
            // fs::copy preserves rwx but drops special bits (setuid/setgid/sticky);
            // reapply the full source mode so e.g. setuid-root binaries survive.
            if let Ok(meta) = std::fs::metadata(&src_path) {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &dst_path,
                    std::fs::Permissions::from_mode(meta.permissions().mode()),
                );
            }
        }
    }
    Ok(())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Write `package.json` (a runix-package-format manifest) beside a package's
/// output, recording every file with its content hash + ELF needs so RuneForge
/// can compose by package set + dependency closure.
pub fn emit_package_manifest(meta: &crate::config::PackageMeta, pkg_output: &Path) -> Result<(), String> {
    let core = pkg_output.join("Core");
    let build_only = meta
        .extra
        .get("build_only")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| build_only_heuristic(&meta.name));
    // A meta-package (base-image) is a union of other packages' files; flag it so
    // dependency resolution picks the real owning package, not the union.
    let is_meta = meta
        .extra
        .get("meta")
        .and_then(|v| v.as_bool())
        .unwrap_or(meta.name == "base-image");
    let manifest = runix_package_format::scan::scan_core(
        &core,
        &meta.name,
        &meta.version,
        &meta.description,
        meta.dependencies.clone(),
        build_only,
        is_meta,
    )
    .map_err(|e| format!("scan package manifest: {}", e))?;
    std::fs::write(pkg_output.join("package.json"), manifest.to_json())
        .map_err(|e| format!("write package.json: {}", e))?;
    println!(
        "  Manifest: {} files{}",
        manifest.files.len(),
        if build_only { " (build-only)" } else { "" }
    );
    Ok(())
}

/// Toolchains and headers are build-only (never shipped in a runtime /Core).
/// Runtime libraries (e.g. llvm-runtimes -> libunwind) are NOT build-only.
/// Overridable per package via `build_only = true` in meta.toml.
fn build_only_heuristic(name: &str) -> bool {
    name.ends_with("-native")
        || matches!(
            name,
            "llvm" | "llvm21" | "compiler-rt" | "cmake-native" | "kernel-headers"
                | "glibc-headers" | "rust" | "lmtest"
        )
}
