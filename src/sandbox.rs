use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, unshare};
use nix::unistd::{chroot, chdir, sethostname};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Mount points to bind into the sandbox
const BIND_MOUNTS: &[(&str, &str)] = &[
    ("/dev", "dev"),
    ("/proc", "proc"),
    ("/sys", "sys"),
    // Bind host utilities until RunixOS has its own coreutils/shell
    ("/usr/bin", "host/bin"),
    ("/usr/lib", "host/lib"),
    ("/lib", "host/syslib"),
];

/// Set up the sandbox filesystem by bind-mounting kernel interfaces
pub fn setup_mounts(sysroot: &Path) -> Result<(), String> {
    // Ensure mount points exist
    for (_, target) in BIND_MOUNTS {
        let full = sysroot.join(target);
        std::fs::create_dir_all(&full)
            .map_err(|e| format!("mkdir {:?}: {}", full, e))?;
    }

    // Create Transit directories
    let ephemeral = sysroot.join("Transit/Ephemeral");
    std::fs::create_dir_all(&ephemeral)
        .map_err(|e| format!("mkdir Transit/Ephemeral: {}", e))?;

    // Bind mount kernel interfaces
    for (source, target) in BIND_MOUNTS {
        let full = sysroot.join(target);
        mount(
            Some(*source),
            &full,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        ).map_err(|e| format!("bind mount {} -> {:?}: {}", source, full, e))?;
    }

    // Mount tmpfs for /Transit/Ephemeral
    mount(
        Some("tmpfs"),
        &ephemeral,
        Some("tmpfs"),
        MsFlags::empty(),
        Some("size=512M"),
    ).map_err(|e| format!("tmpfs Transit/Ephemeral: {}", e))?;

    Ok(())
}

/// Clean up mounts (called on normal exit; on crash, PID namespace handles it)
pub fn cleanup_mounts(sysroot: &Path) {
    let ephemeral = sysroot.join("Transit/Ephemeral");
    let _ = umount2(&ephemeral, MntFlags::MNT_DETACH);

    for (_, target) in BIND_MOUNTS.iter().rev() {
        let full = sysroot.join(target);
        let _ = umount2(&full, MntFlags::MNT_DETACH);
    }
}

/// Enter the sandbox with chroot (root mode)
fn enter_chroot(sysroot: &Path, cmd: &[&str], envs: &[(&str, &str)]) -> Result<i32, String> {
    // Create new mount + PID namespace
    unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID)
        .map_err(|e| format!("unshare: {}", e))?;

    setup_mounts(sysroot)?;

    // Fork - child becomes PID 1 in new namespace
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child, .. }) => {
            // Wait for child
            let status = nix::sys::wait::waitpid(child, None)
                .map_err(|e| format!("waitpid: {}", e))?;
            cleanup_mounts(sysroot);
            match status {
                nix::sys::wait::WaitStatus::Exited(_, code) => Ok(code),
                _ => Ok(1),
            }
        }
        Ok(nix::unistd::ForkResult::Child) => {
            chroot(sysroot).map_err(|e| format!("chroot: {}", e))?;
            chdir("/").map_err(|e| format!("chdir /: {}", e))?;
            let _ = sethostname("runixos");

            let mut command = Command::new(cmd[0]);
            if cmd.len() > 1 {
                command.args(&cmd[1..]);
            }
            command.env_clear();
            command.env("PATH", "/Core/Bin:/Construct/Bin:/host/bin");
            command.env("HOME", "/Space/builder");
            command.env("TERM", std::env::var("TERM").unwrap_or("xterm".into()));
            command.env("LD_LIBRARY_PATH", "/Core/LibKit:/Construct/LibKit:/host/lib:/host/syslib");
            for (k, v) in envs {
                command.env(k, v);
            }

            let status = command.status()
                .map_err(|e| format!("exec {:?}: {}", cmd[0], e))?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Err(e) => Err(format!("fork: {}", e)),
    }
}

/// Enter the sandbox with user namespaces (non-root mode)
/// Without root, we can't chroot or bind mount. Instead, we set up
/// environment variables so tools find RunixOS sysroot paths, and use
/// PID namespace for process isolation.
fn enter_userns(sysroot: &Path, cmd: &[&str], envs: &[(&str, &str)]) -> Result<i32, String> {
    let sysroot_str = sysroot.to_str().unwrap();

    // RunixOS binaries have interpreter /Core/LibKit/ld-runixos-x86-64.rdl.2
    // which doesn't exist on the host. Create wrapper scripts that invoke each
    // binary via the dynamic linker so they work without chroot.
    let ld_path = format!("{}/Core/LibKit/ld-runixos-x86-64.rdl.2", sysroot_str);
    let lib_path = format!("{}/Core/LibKit", sysroot_str);
    let bin_dir = sysroot.join("Core/Bin");
    let has_ld = std::path::Path::new(&ld_path).exists();

    let wrapper_dir = std::env::temp_dir().join("runixos-wrappers");
    if has_ld && bin_dir.exists() {
        let _ = std::fs::remove_dir_all(&wrapper_dir);
        std::fs::create_dir_all(&wrapper_dir)
            .map_err(|e| format!("mkdir wrappers: {}", e))?;

        // Create a wrapper for each binary/symlink in Core/Bin
        if let Ok(entries) = std::fs::read_dir(&bin_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                let target_bin = entry.path();
                let wrapper = wrapper_dir.join(&*name);
                let script = format!(
                    "#!/bin/sh\nexec \"{}\" --library-path \"{}\" \"{}\" \"$@\"\n",
                    ld_path, lib_path, target_bin.display()
                );
                let _ = std::fs::write(&wrapper, script);
                let _ = std::fs::set_permissions(
                    &wrapper,
                    std::os::unix::fs::PermissionsExt::from_mode(0o755),
                );
                // Skip if it's a host tool (like clang) that runs natively
                if name_str == "clang" || name_str == "clang++" || name_str.starts_with("llvm-")
                    || name_str == "cmake" || name_str == "lld" || name_str == "ninja"
                {
                    let _ = std::fs::remove_file(&wrapper);
                }
            }
        }
    }

    // Resolve command
    let real_cmd = if cmd[0] == "/host/bin/sh" {
        "/bin/sh".to_string()
    } else if cmd[0].starts_with("/Core/") || cmd[0].starts_with("/Construct/") {
        // Use wrapper if available
        let name = std::path::Path::new(cmd[0]).file_name().unwrap().to_string_lossy().to_string();
        let wrapper = wrapper_dir.join(&name);
        if wrapper.exists() {
            wrapper.to_string_lossy().to_string()
        } else {
            format!("{}{}", sysroot_str, cmd[0])
        }
    } else {
        cmd[0].to_string()
    };

    let mut command = Command::new(&real_cmd);
    if cmd.len() > 1 {
        command.args(&cmd[1..]);
    }
    command.env_clear();
    // Wrappers first, then host tools as fallback
    let cargo_bin = std::env::var("HOME").map(|h| format!("{}/.cargo/bin", h)).unwrap_or_default();
    command.env("PATH", format!("{}:{}/Core/Bin:{}/Construct/Bin:{}:/usr/bin:/bin",
        wrapper_dir.display(), sysroot_str, sysroot_str, cargo_bin));
    command.env("HOME", std::env::var("HOME").unwrap_or("/tmp".into()));
    command.env("TERM", std::env::var("TERM").unwrap_or("xterm".into()));
    command.env("LD_LIBRARY_PATH", format!("{}/Core/LibKit:{}/Construct/LibKit", sysroot_str, sysroot_str));
    command.env("SYSROOT", sysroot_str);
    command.env("CC", format!("{}/Core/Bin/clang", sysroot_str));
    command.env("CXX", format!("{}/Core/Bin/clang++", sysroot_str));
    command.env("CMAKE", format!("{}/Core/Bin/cmake", sysroot_str));
    command.env("AR", format!("{}/Core/Bin/llvm-ar", sysroot_str));

    // RunixOS Rust cross-compilation
    let rust_build = std::path::Path::new(sysroot_str)
        .parent().unwrap_or(std::path::Path::new("/"))
        .join("coding/rovelos/rust/build/x86_64-unknown-linux-gnu");
    let stage1_rustc = rust_build.join("stage1/bin/rustc");
    let stage1_std = rust_build.join("stage1-std/x86_64-rovelstars-runixos/release/deps");
    if stage1_rustc.exists() {
        command.env("RUNIXOS_RUSTC", stage1_rustc.to_str().unwrap());
        command.env("RUNIXOS_STD_DEPS", stage1_std.to_str().unwrap());
        command.env("CARGO_TARGET_X86_64_ROVELSTARS_RUNIXOS_LINKER",
            format!("{}/Core/Bin/clang", sysroot_str));
        command.env("RUNIXOS_TARGET", "x86_64-rovelstars-runixos");
    }
    // Path to our patched libc crate for RunixOS
    let libc_path = std::path::Path::new(sysroot_str)
        .parent().unwrap_or(std::path::Path::new("/"))
        .join("coding/rovelos/libc");
    if libc_path.exists() {
        command.env("RUNIXOS_LIBC_PATH", libc_path.to_str().unwrap());
    }

    // Cross-compilation CC/CXX for RunixOS target (used by cc-rs)
    command.env("CC_x86_64_rovelstars_runixos", format!("{}/Core/Bin/clang", sysroot_str));
    command.env("CXX_x86_64_rovelstars_runixos", format!("{}/Core/Bin/clang++", sysroot_str));
    command.env("AR_x86_64_rovelstars_runixos", format!("{}/Core/Bin/llvm-ar", sysroot_str));
    command.env("CFLAGS_x86_64_rovelstars_runixos",
        format!("--sysroot={} --target=x86_64-rovelstars-runixos", sysroot_str));

    // Inherit host's HOME for cargo/rustup
    if let Ok(home) = std::env::var("HOME") {
        command.env("CARGO_HOME", format!("{}/.cargo", home));
        command.env("RUSTUP_HOME", format!("{}/.rustup", home));
    }
    for (k, v) in envs {
        command.env(k, v);
    }

    let status = command.status()
        .map_err(|e| format!("exec: {}", e))?;
    Ok(status.code().unwrap_or(1))
}

/// Run a command in the sandbox
pub fn run_in_sandbox(
    sysroot: &Path,
    cmd: &[&str],
    envs: &[(&str, &str)],
    is_root: bool,
) -> Result<i32, String> {
    if is_root {
        enter_chroot(sysroot, cmd, envs)
    } else {
        enter_userns(sysroot, cmd, envs)
    }
}

/// Enter interactive shell in sandbox
pub fn enter_interactive(sysroot: &Path, is_root: bool) -> Result<(), String> {
    // Show RunixOS banner
    if let Ok(release) = std::fs::read_to_string(sysroot.join("Core/Config/OSReleaseInfo")) {
        for line in release.lines() {
            if line.starts_with("PRETTY_NAME=") {
                let name = line.trim_start_matches("PRETTY_NAME=").trim_matches('"');
                println!("  Welcome to {}", name);
                break;
            }
        }
    }

    // Prefer RunixOS shell, fall back to host shell
    let shell = if sysroot.join("Core/Bin/brush").exists() {
        "/Core/Bin/brush"
    } else if sysroot.join("Core/Bin/nu").exists() {
        "/Core/Bin/nu"
    } else {
        "/host/bin/sh"
    };

    let code = run_in_sandbox(sysroot, &[shell], &[], is_root)?;
    if code != 0 {
        Err(format!("Shell exited with code {}", code))
    } else {
        Ok(())
    }
}
