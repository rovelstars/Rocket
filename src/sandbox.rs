use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, chdir, chroot, execve, fork, sethostname};
use std::ffi::CString;
use std::path::Path;
use std::process::Command;

/// Host bind mounts - only used when host links are enabled
const HOST_MOUNTS: &[(&str, &str)] = &[
    ("/usr/bin", "host/bin"),
    ("/usr/lib", "host/lib"),
    ("/lib", "host/syslib"),
];

/// Set up the sandbox filesystem
pub fn setup_mounts(
    sysroot: &Path,
    enable_host_links: bool,
    binds: &[(std::path::PathBuf, String)],
) -> Result<(), String> {
    let dev_dir = sysroot.join("dev");
    std::fs::create_dir_all(&dev_dir).map_err(|e| format!("mkdir dev: {}", e))?;

    // Create Transit directories
    let ephemeral = sysroot.join("Transit/Ephemeral");
    std::fs::create_dir_all(&ephemeral).map_err(|e| format!("mkdir Transit/Ephemeral: {}", e))?;

    // Mount tmpfs for /dev - completely isolated from host /dev
    mount(
        Some("tmpfs"),
        &dev_dir,
        Some("tmpfs"),
        MsFlags::empty(),
        Some("size=65536k,mode=755"),
    )
    .map_err(|e| format!("tmpfs /dev: {}", e))?;

    // Create essential device nodes
    use nix::sys::stat::{Mode, SFlag, mknod};
    let devices: &[(&str, u64)] = &[
        ("null", nix::sys::stat::makedev(1, 3)),
        ("zero", nix::sys::stat::makedev(1, 5)),
        ("full", nix::sys::stat::makedev(1, 7)),
        ("random", nix::sys::stat::makedev(1, 8)),
        ("urandom", nix::sys::stat::makedev(1, 9)),
        ("tty", nix::sys::stat::makedev(5, 0)),
    ];
    for (name, dev) in devices {
        let path = dev_dir.join(name);
        let _ = mknod(&path, SFlag::S_IFCHR, Mode::from_bits_truncate(0o666), *dev);
    }

    // Symlinks
    std::os::unix::fs::symlink("/proc/self/fd", dev_dir.join("fd")).ok();
    std::os::unix::fs::symlink("/proc/self/fd/0", dev_dir.join("stdin")).ok();
    std::os::unix::fs::symlink("/proc/self/fd/1", dev_dir.join("stdout")).ok();
    std::os::unix::fs::symlink("/proc/self/fd/2", dev_dir.join("stderr")).ok();

    // Mount devpts
    std::fs::create_dir_all(dev_dir.join("pts")).map_err(|e| format!("mkdir dev/pts: {}", e))?;
    mount(
        Some("devpts"),
        &dev_dir.join("pts"),
        Some("devpts"),
        MsFlags::empty(),
        Some("newinstance,ptmxmode=0666,mode=620,gid=5"),
    )
    .map_err(|e| format!("mount devpts: {}", e))?;
    std::os::unix::fs::symlink("pts/ptmx", dev_dir.join("ptmx"))
        .map_err(|e| format!("symlink dev/ptmx: {}", e))?;

    // Mount devshm
    std::fs::create_dir_all(dev_dir.join("shm")).map_err(|e| format!("mkdir dev/shm: {}", e))?;
    mount(
        Some("tmpfs"),
        &dev_dir.join("shm"),
        Some("tmpfs"),
        MsFlags::empty(),
        Some("size=64m"),
    )
    .map_err(|e| format!("mount dev/shm: {}", e))?;

    // Mount /proc (bind from host - we're not in a PID namespace)
    let proc_dir = sysroot.join("proc");
    std::fs::create_dir_all(&proc_dir).map_err(|e| format!("mkdir proc: {}", e))?;
    mount(
        Some("/proc"),
        &proc_dir,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| format!("bind mount /proc: {}", e))?;

    // Mount /sys
    let sys_dir = sysroot.join("sys");
    std::fs::create_dir_all(&sys_dir).map_err(|e| format!("mkdir sys: {}", e))?;
    mount(
        Some("/sys"),
        &sys_dir,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| format!("bind mount /sys: {}", e))?;

    // Host tool mounts - only when explicitly requested
    if enable_host_links {
        for (source, target) in HOST_MOUNTS {
            let full = sysroot.join(target);
            std::fs::create_dir_all(&full).map_err(|e| format!("mkdir {:?}: {}", full, e))?;
            mount(
                Some(*source),
                &full,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REC,
                None::<&str>,
            )
            .map_err(|e| format!("bind mount {} -> {:?}: {}", source, full, e))?;
        }

        // Symlink so host ELF binaries can find their interpreter
        let lib64 = sysroot.join("lib64");
        std::fs::create_dir_all(&lib64).ok();
        std::os::unix::fs::symlink(
            "/host/syslib/ld-linux-x86-64.so.2",
            lib64.join("ld-linux-x86-64.so.2"),
        )
        .ok();
    }

    // Caller-requested bind mounts (e.g. a local source tree for --local).
    // Targets live under Transit/Build (real disk), not the tmpfs below.
    for (source, target) in binds {
        let full = sysroot.join(target);
        std::fs::create_dir_all(&full).map_err(|e| format!("mkdir bind {:?}: {}", full, e))?;
        mount(
            Some(source.as_path()),
            &full,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(|e| format!("bind mount {:?} -> {:?}: {}", source, full, e))?;
    }

    // Mount tmpfs for /Transit/Ephemeral
    mount(
        Some("tmpfs"),
        &ephemeral,
        Some("tmpfs"),
        MsFlags::empty(),
        Some("size=512M"),
    )
    .map_err(|e| format!("tmpfs Transit/Ephemeral: {}", e))?;

    Ok(())
}

// No manual cleanup needed - mounts are in a private namespace that the
// kernel destroys when the child process exits. This is crash-safe:
// even if the sandbox process is killed, no host mounts are affected.

/// Enter the sandbox with chroot (root mode).
///
/// Interactive shells exec in place (no fork) so they keep the terminal
/// session/process group for job control. Non-interactive builds fork: the
/// child sets up the namespace + chroot and execs the build, while the parent
/// waits and returns the exit code - so the caller regains control to copy out
/// artifacts. The mount namespace is private and torn down by the kernel when
/// the child exits.
fn enter_chroot(
    sysroot_raw: &Path,
    cmd: &[&str],
    envs: &[(&str, &str)],
    enable_host_links: bool,
    interactive: bool,
    binds: &[(std::path::PathBuf, String)],
) -> Result<i32, String> {
    let sysroot = sysroot_raw
        .canonicalize()
        .map_err(|e| format!("canonicalize sysroot {:?}: {}", sysroot_raw, e))?;

    if interactive {
        do_chroot_exec(&sysroot, cmd, envs, enable_host_links, binds)?;
        unreachable!();
    }

    match unsafe { fork() }.map_err(|e| format!("fork: {}", e))? {
        ForkResult::Child => {
            if let Err(e) = do_chroot_exec(&sysroot, cmd, envs, enable_host_links, binds) {
                eprintln!("sandbox child: {}", e);
                std::process::exit(127);
            }
            unreachable!();
        }
        ForkResult::Parent { child } => match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => Ok(code),
            Ok(WaitStatus::Signaled(_, sig, _)) => Ok(128 + sig as i32),
            Ok(_) => Ok(1),
            Err(e) => Err(format!("waitpid: {}", e)),
        },
    }
}

/// Set up the namespace + chroot in the current process and exec `cmd`.
/// On success this never returns (execve replaces the process image).
fn do_chroot_exec(
    sysroot: &Path,
    cmd: &[&str],
    envs: &[(&str, &str)],
    enable_host_links: bool,
    binds: &[(std::path::PathBuf, String)],
) -> Result<i32, String> {

    // Enter isolated mount + UTS namespace (no fork needed -
    // unshare only affects this process, and mounts are private)
    unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUTS)
        .map_err(|e| format!("unshare: {}", e))?;

    // Make all existing mounts private so our bind mounts
    // don't propagate back to the host
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|e| format!("make-private /: {}", e))?;

    setup_mounts(sysroot, enable_host_links, binds)?;

    chroot(sysroot).map_err(|e| format!("chroot: {}", e))?;
    chdir("/").map_err(|e| format!("chdir /: {}", e))?;
    let _ = sethostname("runixos");

    // Build argv for execve
    let argv: Vec<CString> = cmd.iter().map(|s| CString::new(*s).unwrap()).collect();

    // Build envp for execve
    let term = std::env::var("TERM").unwrap_or("xterm".into());
    let mut env_strings = vec![format!("HOME=/Space/builder"), format!("TERM={}", term)];

    if enable_host_links {
        env_strings.push(format!("PATH=/Core/Bin:/Construct/Bin:/host/bin"));
        env_strings.push(format!(
            "LD_LIBRARY_PATH=/Core/LibKit:/Construct/LibKit:/host/lib:/host/syslib"
        ));
    } else {
        env_strings.push(format!("PATH=/Core/Bin:/Construct/Bin"));
        env_strings.push(format!("LD_LIBRARY_PATH=/Core/LibKit:/Construct/LibKit"));
    }

    for (k, v) in envs {
        env_strings.push(format!("{}={}", k, v));
    }
    let envp: Vec<CString> = env_strings
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect();

    // execve replaces this process with the shell - terminal
    // session and process group are inherited naturally
    execve(&argv[0], &argv, &envp)
        .map_err(|e| format!("execve {:?}: {}", cmd[0], e))?;
    unreachable!();
}

/// Enter the sandbox with user namespaces (non-root mode)
/// Without root, we can't chroot or bind mount. Instead, we set up
/// environment variables so tools find RunixOS sysroot paths.
fn enter_userns(
    sysroot: &Path,
    cmd: &[&str],
    envs: &[(&str, &str)],
    enable_host_links: bool,
) -> Result<i32, String> {
    // Canonicalize to absolute path so wrapper scripts work regardless of cwd
    let sysroot = sysroot
        .canonicalize()
        .map_err(|e| format!("canonicalize sysroot {:?}: {}", sysroot, e))?;
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
        std::fs::create_dir_all(&wrapper_dir).map_err(|e| format!("mkdir wrappers: {}", e))?;

        // Create a wrapper for each binary/symlink in Core/Bin
        if let Ok(entries) = std::fs::read_dir(&bin_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                let target_bin = entry.path();
                let wrapper = wrapper_dir.join(&*name);
                let script = format!(
                    "#!/bin/sh\nexec \"{}\" --library-path \"{}\" \"{}\" \"$@\"\n",
                    ld_path,
                    lib_path,
                    target_bin.display()
                );
                let _ = std::fs::write(&wrapper, script);
                let _ = std::fs::set_permissions(
                    &wrapper,
                    std::os::unix::fs::PermissionsExt::from_mode(0o755),
                );
                // Skip if it's a host tool (like clang) that runs natively
                if name_str == "clang"
                    || name_str == "clang++"
                    || name_str.starts_with("llvm-")
                    || name_str == "cmake"
                    || name_str == "lld"
                    || name_str == "ninja"
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
        let name = std::path::Path::new(cmd[0])
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
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

    if enable_host_links {
        let cargo_bin = std::env::var("HOME")
            .map(|h| format!("{}/.cargo/bin", h))
            .unwrap_or_default();
        command.env(
            "PATH",
            format!(
                "{}:{}/Core/Bin:{}/Construct/Bin:{}:/usr/bin:/bin",
                wrapper_dir.display(),
                sysroot_str,
                sysroot_str,
                cargo_bin
            ),
        );
        command.env(
            "LD_LIBRARY_PATH",
            format!(
                "{}/Core/LibKit:{}/Construct/LibKit",
                sysroot_str, sysroot_str
            ),
        );

        // Cross-compilation env vars for builds
        command.env("CC", format!("{}/Core/Bin/clang", sysroot_str));
        command.env("CXX", format!("{}/Core/Bin/clang++", sysroot_str));
        command.env("CMAKE", format!("{}/Core/Bin/cmake", sysroot_str));
        command.env("AR", format!("{}/Core/Bin/llvm-ar", sysroot_str));

        // RunixOS Rust cross-compilation
        let rust_build = std::path::Path::new(sysroot_str)
            .parent()
            .unwrap_or(std::path::Path::new("/"))
            .join("coding/rovelos/rust/build/x86_64-unknown-linux-gnu");
        let stage1_rustc = rust_build.join("stage1/bin/rustc");
        let stage1_std = rust_build.join("stage1-std/x86_64-rovelstars-linux-runixos/release/deps");
        if stage1_rustc.exists() {
            command.env("RUNIXOS_RUSTC", stage1_rustc.to_str().unwrap());
            command.env("RUNIXOS_STD_DEPS", stage1_std.to_str().unwrap());
            command.env(
                "CARGO_TARGET_X86_64_ROVELSTARS_LINUX_RUNIXOS_LINKER",
                format!("{}/Core/Bin/clang", sysroot_str),
            );
            command.env("RUNIXOS_TARGET", "x86_64-rovelstars-linux-runixos");
        }
        let libc_path = std::path::Path::new(sysroot_str)
            .parent()
            .unwrap_or(std::path::Path::new("/"))
            .join("coding/rovelos/libc");
        if libc_path.exists() {
            command.env("RUNIXOS_LIBC_PATH", libc_path.to_str().unwrap());
        }

        command.env(
            "CC_x86_64_rovelstars_linux_runixos",
            format!("{}/Core/Bin/clang", sysroot_str),
        );
        command.env(
            "CXX_x86_64_rovelstars_linux_runixos",
            format!("{}/Core/Bin/clang++", sysroot_str),
        );
        command.env(
            "AR_x86_64_rovelstars_linux_runixos",
            format!("{}/Core/Bin/llvm-ar", sysroot_str),
        );
        command.env(
            "CFLAGS_x86_64_rovelstars_linux_runixos",
            format!(
                "--sysroot={} --target=x86_64-rovelstars-linux-runixos",
                sysroot_str
            ),
        );

        // Inherit host's HOME for cargo/rustup
        if let Ok(home) = std::env::var("HOME") {
            command.env("CARGO_HOME", format!("{}/.cargo", home));
            command.env("RUSTUP_HOME", format!("{}/.rustup", home));
        }
    } else {
        command.env(
            "PATH",
            format!(
                "{}:{}/Core/Bin:{}/Construct/Bin",
                wrapper_dir.display(),
                sysroot_str,
                sysroot_str
            ),
        );
        command.env(
            "LD_LIBRARY_PATH",
            format!(
                "{}/Core/LibKit:{}/Construct/LibKit",
                sysroot_str, sysroot_str
            ),
        );
    }

    command.env("HOME", std::env::var("HOME").unwrap_or("/tmp".into()));
    command.env("TERM", std::env::var("TERM").unwrap_or("xterm".into()));
    command.env("SYSROOT", sysroot_str);

    for (k, v) in envs {
        command.env(k, v);
    }

    let status = command.status().map_err(|e| format!("exec: {}", e))?;
    Ok(status.code().unwrap_or(1))
}

/// Run a command in the sandbox
pub fn run_in_sandbox(
    sysroot: &Path,
    cmd: &[&str],
    envs: &[(&str, &str)],
    is_root: bool,
    enable_host_links: bool,
    interactive: bool,
    binds: &[(std::path::PathBuf, String)],
) -> Result<i32, String> {
    if is_root {
        enter_chroot(sysroot, cmd, envs, enable_host_links, interactive, binds)
    } else {
        // Non-root mode runs natively (no chroot); binds are unnecessary
        // because build.sh sees host paths directly via $LOCAL_SRC.
        enter_userns(sysroot, cmd, envs, enable_host_links)
    }
}

/// Enter interactive shell in sandbox
pub fn enter_interactive(
    sysroot: &Path,
    is_root: bool,
    enable_host_links: bool,
) -> Result<(), String> {
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

    // Prefer RunixOS shell, fall back to host shell (only if host links enabled)
    let (shell, shell_args): (&str, &[&str]) = if sysroot.join("Core/Bin/brush").exists() {
        ("/Core/Bin/brush", &["--login", "-i"])
    } else if sysroot.join("Core/Bin/nu").exists() {
        ("/Core/Bin/nu", &[])
    } else if enable_host_links {
        ("/host/bin/bash", &["-i"])
    } else {
        return Err(
            "No RunixOS shell found (brush or nu). Use --enable-host-links for host fallback."
                .into(),
        );
    };

    let mut cmd = vec![shell];
    cmd.extend_from_slice(shell_args);
    // Interactive: exec in place to keep job control. No binds.
    let code = run_in_sandbox(sysroot, &cmd, &[], is_root, enable_host_links, true, &[])?;
    if code != 0 {
        Err(format!("Shell exited with code {}", code))
    } else {
        Ok(())
    }
}
