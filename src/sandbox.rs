use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, chdir, chroot, execve, fork, getgid, getuid, sethostname};
use std::ffi::CString;
use std::path::Path;

/// Host directories bind-mounted read-only into the sandbox so host build tools
/// (gcc, make, cmake, curl, coreutils) and their shared libraries are available
/// at the usual paths. The RunixOS cross toolchain itself lives in the sysroot
/// at /Core/Bin. Cross-building needs both: host tools to drive the build, the
/// sysroot clang to emit RunixOS code.
const HOST_RO_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt"];
/// Host files bind-mounted read-only so network fetches (git clone, curl) work.
const HOST_RO_FILES: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/pki",
];

/// We run unprivileged: a user namespace maps our real uid/gid to root inside,
/// which lets us chroot + mount without sudo. The kernel tears the namespace
/// (and all its mounts) down when the process exits, so it is crash-safe and
/// never touches the host. proot/ptrace is not used (it would trap every
/// syscall and cripple compile-heavy builds); chroot is native speed.
fn write_id_maps(uid: u32, gid: u32) -> Result<(), String> {
    // setgroups must be denied before gid_map can be written unprivileged.
    std::fs::write("/proc/self/setgroups", "deny")
        .map_err(|e| format!("setgroups deny: {}", e))?;
    std::fs::write("/proc/self/uid_map", format!("0 {} 1", uid))
        .map_err(|e| format!("uid_map: {}", e))?;
    std::fs::write("/proc/self/gid_map", format!("0 {} 1", gid))
        .map_err(|e| format!("gid_map: {}", e))?;
    Ok(())
}

fn bind(src: &Path, dst: &Path, recursive: bool) -> Result<(), String> {
    let mut flags = MsFlags::MS_BIND;
    if recursive {
        flags |= MsFlags::MS_REC;
    }
    mount(Some(src), dst, None::<&str>, flags, None::<&str>)
        .map_err(|e| format!("bind {:?} -> {:?}: {}", src, dst, e))
}

fn bind_ro(src: &Path, dst: &Path) -> Result<(), String> {
    bind(src, dst, true)?;
    // Remount read-only so a build cannot scribble on host files.
    mount(
        Some(src),
        dst,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .map_err(|e| format!("remount-ro {:?}: {}", dst, e))
}

/// Set up the sandbox filesystem inside the (already private) mount namespace.
pub fn setup_mounts(
    sysroot: &Path,
    host_links: bool,
    binds: &[(std::path::PathBuf, String)],
) -> Result<(), String> {
    // /dev: a user namespace cannot mknod real device nodes, so recursively
    // bind the host /dev (gives null, zero, urandom, tty, pts, ... for free).
    let dev_dir = sysroot.join("dev");
    std::fs::create_dir_all(&dev_dir).map_err(|e| format!("mkdir dev: {}", e))?;
    bind(Path::new("/dev"), &dev_dir, true)?;

    // /proc and /sys bound from the host (no PID namespace).
    for (src, sub) in [("/proc", "proc"), ("/sys", "sys")] {
        let dst = sysroot.join(sub);
        std::fs::create_dir_all(&dst).map_err(|e| format!("mkdir {}: {}", sub, e))?;
        bind(Path::new(src), &dst, true)?;
    }

    if host_links {
        for dir in HOST_RO_DIRS {
            let src = Path::new(dir);
            if !src.exists() {
                continue;
            }
            let dst = sysroot.join(dir.trim_start_matches('/'));
            std::fs::create_dir_all(&dst).map_err(|e| format!("mkdir {:?}: {}", dst, e))?;
            bind_ro(src, &dst)?;
        }
        // Network/cert files for git clone + curl downloads.
        let etc = sysroot.join("etc");
        std::fs::create_dir_all(&etc).map_err(|e| format!("mkdir etc: {}", e))?;
        for f in HOST_RO_FILES {
            let src = Path::new(f);
            if !src.exists() {
                continue;
            }
            let dst = sysroot.join(f.trim_start_matches('/'));
            if src.is_dir() {
                std::fs::create_dir_all(&dst).ok();
            } else {
                if let Some(p) = dst.parent() {
                    std::fs::create_dir_all(p).ok();
                }
                let _ = std::fs::File::create(&dst);
            }
            // Best-effort: a missing cert dir should not abort the build.
            let _ = bind_ro(src, &dst);
        }
    }

    // Caller-requested bind mounts (e.g. a local source tree for --local).
    for (source, target) in binds {
        let full = sysroot.join(target);
        std::fs::create_dir_all(&full).map_err(|e| format!("mkdir bind {:?}: {}", full, e))?;
        bind(source.as_path(), &full, true)?;
    }

    // Writable scratch for /Transit/Ephemeral (the rest of Transit is real disk).
    let ephemeral = sysroot.join("Transit/Ephemeral");
    std::fs::create_dir_all(&ephemeral).map_err(|e| format!("mkdir Transit/Ephemeral: {}", e))?;
    mount(
        Some("tmpfs"),
        &ephemeral,
        Some("tmpfs"),
        MsFlags::empty(),
        Some("size=512M"),
    )
    .map_err(|e| format!("tmpfs Transit/Ephemeral: {}", e))?;

    // A writable /tmp (1777). Host build tools (clang, configure scripts) create
    // temp files in /tmp by default, and the chroot's sysroot has no /tmp of its
    // own. Without this, clang fails with "unable to make temporary file".
    let tmp = sysroot.join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| format!("mkdir tmp: {}", e))?;
    mount(
        Some("tmpfs"),
        &tmp,
        Some("tmpfs"),
        MsFlags::empty(),
        Some("size=4G,mode=1777"),
    )
    .map_err(|e| format!("tmpfs /tmp: {}", e))?;

    Ok(())
}

/// Enter the sandbox: unprivileged user namespace + chroot, then exec `cmd`.
///
/// Interactive shells exec in place (no fork) so they keep the terminal session
/// and process group for job control. Non-interactive builds fork: the child
/// sets up the namespace + chroot and execs, the parent waits and returns the
/// exit code so the caller regains control to copy out artifacts. fork also
/// guarantees the unshare(CLONE_NEWUSER) runs in a single-threaded process,
/// which the kernel requires.
fn enter_sandbox(
    sysroot_raw: &Path,
    cmd: &[&str],
    envs: &[(&str, &str)],
    host_links: bool,
    interactive: bool,
    binds: &[(std::path::PathBuf, String)],
) -> Result<i32, String> {
    let sysroot = sysroot_raw
        .canonicalize()
        .map_err(|e| format!("canonicalize sysroot {:?}: {}", sysroot_raw, e))?;

    if interactive {
        do_enter(&sysroot, cmd, envs, host_links, binds)?;
        unreachable!();
    }

    match unsafe { fork() }.map_err(|e| format!("fork: {}", e))? {
        ForkResult::Child => {
            if let Err(e) = do_enter(&sysroot, cmd, envs, host_links, binds) {
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

/// Create the user + mount namespace, set up the filesystem, chroot, exec.
/// On success this never returns (execve replaces the process image).
fn do_enter(
    sysroot: &Path,
    cmd: &[&str],
    envs: &[(&str, &str)],
    host_links: bool,
    binds: &[(std::path::PathBuf, String)],
) -> Result<i32, String> {
    let uid = getuid().as_raw();
    let gid = getgid().as_raw();

    // User namespace (so we can chroot+mount unprivileged) + mount + UTS.
    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUTS)
        .map_err(|e| format!("unshare (unprivileged user namespaces enabled?): {}", e))?;
    write_id_maps(uid, gid)?;

    // Make all mounts private so our binds never propagate back to the host.
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|e| format!("make-private /: {}", e))?;

    setup_mounts(sysroot, host_links, binds)?;

    chroot(sysroot).map_err(|e| format!("chroot: {}", e))?;
    chdir("/").map_err(|e| format!("chdir /: {}", e))?;
    let _ = sethostname("runixos");

    let argv: Vec<CString> = cmd.iter().map(|s| CString::new(*s).unwrap()).collect();

    let term = std::env::var("TERM").unwrap_or("xterm".into());
    let path = if host_links {
        "/Core/Bin:/Construct/Bin:/usr/bin:/bin"
    } else {
        "/Core/Bin:/Construct/Bin"
    };
    let mut env_strings = vec![
        "HOME=/Space/builder".to_string(),
        format!("TERM={}", term),
        format!("PATH={}", path),
    ];
    for (k, v) in envs {
        env_strings.push(format!("{}={}", k, v));
    }
    let envp: Vec<CString> = env_strings
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect();

    execve(&argv[0], &argv, &envp).map_err(|e| format!("execve {:?}: {}", cmd[0], e))?;
    unreachable!();
}

/// Run a command in the sandbox (unprivileged userns + chroot).
pub fn run_in_sandbox(
    sysroot: &Path,
    cmd: &[&str],
    envs: &[(&str, &str)],
    host_links: bool,
    interactive: bool,
    binds: &[(std::path::PathBuf, String)],
) -> Result<i32, String> {
    enter_sandbox(sysroot, cmd, envs, host_links, interactive, binds)
}

/// Enter an interactive shell in the sandbox.
pub fn enter_interactive(sysroot: &Path, host_links: bool) -> Result<(), String> {
    if let Ok(release) = std::fs::read_to_string(sysroot.join("Core/Config/OSReleaseInfo")) {
        for line in release.lines() {
            if let Some(name) = line.strip_prefix("PRETTY_NAME=") {
                println!("  Welcome to {}", name.trim_matches('"'));
                break;
            }
        }
    }

    let (shell, shell_args): (&str, &[&str]) = if sysroot.join("Core/Bin/brush").exists() {
        ("/Core/Bin/brush", &["--login", "-i"])
    } else if sysroot.join("Core/Bin/nu").exists() {
        ("/Core/Bin/nu", &[])
    } else if host_links {
        ("/bin/bash", &["-i"])
    } else {
        return Err(
            "No RunixOS shell found (brush or nu). Use --enable-host-links for a host fallback."
                .into(),
        );
    };

    let mut cmd = vec![shell];
    cmd.extend_from_slice(shell_args);
    let code = run_in_sandbox(sysroot, &cmd, &[], host_links, true, &[])?;
    if code != 0 {
        Err(format!("Shell exited with code {}", code))
    } else {
        Ok(())
    }
}
