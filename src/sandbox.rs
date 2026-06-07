use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, chdir, chroot, execve, fork, getgid, getuid};
use std::ffi::CString;
use std::path::Path;

/// Host directories bind-mounted read-only into the sandbox so host build tools
/// (gcc, make, cmake, curl, coreutils) and their shared libraries are available
/// at the usual paths. The RunixOS cross toolchain itself lives in the sysroot
/// at /Core/Bin. Cross-building needs both: host tools to drive the build, the
/// sysroot clang to emit RunixOS code.
const HOST_RO_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt"];
/// Network config bind-mounted ALWAYS (even without host links) so DNS works for
/// RunixOS's own git/curl inside the sandbox. RunixOS ships its own CA bundle
/// (/Core/Config/ssl/cert.pem) so the host trust store is not needed here.
const HOST_NET_FILES: &[&str] = &["/etc/resolv.conf", "/etc/hosts"];
/// Host trust store, bound only with host links (RunixOS ships its own).
const HOST_RO_FILES: &[&str] = &["/etc/ssl", "/etc/ca-certificates", "/etc/pki"];

/// Build-time autotools site defaults: map the stock share/libexec/man/doc into
/// the RunixOS layout (StoreRoom/LibKit) for every `configure`, so no package
/// needs per-build --datarootdir/--libexecdir flags. `${prefix}` is expanded by
/// configure once --prefix is known.
const CONFIG_SITE: &str = "\
datarootdir='${prefix}/StoreRoom'
datadir='${prefix}/StoreRoom'
libexecdir='${prefix}/LibKit'
mandir='${prefix}/StoreRoom/Manual'
docdir='${prefix}/StoreRoom/Docs'
infodir='${prefix}/StoreRoom/Info'
localedir='${prefix}/StoreRoom/locale'
";

/// We run unprivileged: a user namespace maps our real uid/gid to root inside,
/// which lets us chroot + mount without sudo. The kernel tears the namespace
/// (and all its mounts) down when the process exits, so it is crash-safe and
/// never touches the host. proot/ptrace is not used (it would trap every
/// syscall and cripple compile-heavy builds); chroot is native speed.
fn write_id_maps(
    inside_uid: u32,
    inside_gid: u32,
    host_uid: u32,
    host_gid: u32,
) -> Result<(), String> {
    // setgroups must be denied before gid_map can be written unprivileged.
    // `inside_*` is the uid/gid the process holds inside the namespace: 0 (root)
    // for builds/OOBE, or the logged-in user's id for an interactive session so
    // it actually runs as that account (whoami, file ownership, ...).
    std::fs::write("/proc/self/setgroups", "deny").map_err(|e| format!("setgroups deny: {}", e))?;
    std::fs::write(
        "/proc/self/uid_map",
        format!("{} {} 1", inside_uid, host_uid),
    )
    .map_err(|e| format!("uid_map: {}", e))?;
    std::fs::write(
        "/proc/self/gid_map",
        format!("{} {} 1", inside_gid, host_gid),
    )
    .map_err(|e| format!("gid_map: {}", e))?;
    Ok(())
}

/// Remove the empty stock-FHS mount points the sandbox leaves in the sysroot
/// after a build (host-tool binds + /tmp). `remove_dir` only deletes empty
/// directories, so anything real is left alone.
fn cleanup_stock_dirs(sysroot: &Path) {
    for d in ["bin", "sbin", "lib", "lib64", "usr", "opt", "etc", "tmp"] {
        let _ = std::fs::remove_dir(sysroot.join(d));
    }
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

/// Bind a set of host /etc files/dirs read-only into the sysroot (best-effort:
/// a missing source or failed bind never aborts the build).
fn bind_etc_files(sysroot: &Path, files: &[&str]) -> Result<(), String> {
    std::fs::create_dir_all(sysroot.join("etc")).map_err(|e| format!("mkdir etc: {}", e))?;
    for f in files {
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
        let _ = bind_ro(src, &dst);
    }
    Ok(())
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

    // RunixOS itself has no /etc, and its glibc reads /Core/Config/resolv.conf
    // (the patched _PATH_RESCONF). So bridge the host DNS there for RunixOS's own
    // git/curl - no /etc involved.
    let host_resolv = Path::new("/etc/resolv.conf");
    if host_resolv.exists() {
        let dst = sysroot.join("Core/Config/resolv.conf");
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p).ok();
        }
        if !dst.exists() {
            let _ = std::fs::File::create(&dst);
        }
        let _ = bind_ro(host_resolv, &dst);
    }

    // Host build tools (gcc/git/curl) hardcode /etc paths, so under host-links we
    // still need /etc - but mount it on a tmpfs so nothing the sandbox writes
    // there persists on the real sysroot (it leaves only an empty /etc mount
    // point, never a tree of stub files, and never touches a non-host-links run).
    if host_links {
        let etc = sysroot.join("etc");
        std::fs::create_dir_all(&etc).map_err(|e| format!("mkdir etc: {}", e))?;
        mount(Some("tmpfs"), &etc, Some("tmpfs"), MsFlags::empty(), Some("size=4M"))
            .map_err(|e| format!("tmpfs /etc: {}", e))?;
        bind_etc_files(sysroot, HOST_NET_FILES)?;
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
        // Host trust store (RunixOS ships its own CA bundle, so only with links).
        bind_etc_files(sysroot, HOST_RO_FILES)?;
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

    // Autotools config.site: the proper, global way to keep installs out of the
    // stock $prefix/share and $prefix/libexec. Every autotools `configure` reads
    // $CONFIG_SITE (set in the sandbox env) and adopts the RunixOS layout
    // (StoreRoom/LibKit) without per-package flags. Written to the ephemeral
    // tmpfs so it never persists in the sysroot. The cmake fork already does the
    // equivalent via its RunixOS platform.
    let _ = std::fs::write(ephemeral.join("config.site"), CONFIG_SITE);

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
    login: Option<(u32, u32)>,
) -> Result<i32, String> {
    let sysroot = sysroot_raw
        .canonicalize()
        .map_err(|e| format!("canonicalize sysroot {:?}: {}", sysroot_raw, e))?;

    if interactive {
        do_enter(&sysroot, cmd, envs, host_links, binds, login)?;
        unreachable!();
    }

    match unsafe { fork() }.map_err(|e| format!("fork: {}", e))? {
        ForkResult::Child => {
            if let Err(e) = do_enter(&sysroot, cmd, envs, host_links, binds, login) {
                eprintln!("sandbox child: {}", e);
                std::process::exit(127);
            }
            unreachable!();
        }
        ForkResult::Parent { child } => {
            let r = match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => Ok(code),
                Ok(WaitStatus::Signaled(_, sig, _)) => Ok(128 + sig as i32),
                Ok(_) => Ok(1),
                Err(e) => Err(format!("waitpid: {}", e)),
            };
            // The sandbox creates stock-FHS mount points (host /usr, /bin, ...,
            // /etc, /tmp) in the real sysroot to bind onto. Once the namespace is
            // gone they are empty; remove them so the sysroot keeps only the
            // RunixOS layout. remove_dir only succeeds when empty, so a legit
            // non-empty dir is never touched. dev/proc/sys are kept (RunixOS
            // ships those).
            cleanup_stock_dirs(&sysroot);
            r
        }
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
    login: Option<(u32, u32)>,
) -> Result<i32, String> {
    let uid = getuid().as_raw();
    let gid = getgid().as_raw();

    // User namespace (so we can chroot+mount unprivileged) + mount + UTS.
    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUTS)
        .map_err(|e| format!("unshare (unprivileged user namespaces enabled?): {}", e))?;
    // Builds/OOBE run as inside-root (0); an interactive login runs as the
    // account's uid/gid. The namespace creator keeps full caps either way, so
    // chroot+mount still work even when inside-uid is non-zero.
    let (in_uid, in_gid) = login.unwrap_or((0, 0));
    write_id_maps(in_uid, in_gid, uid, gid)?;

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

    // The build sandbox runs as root with HOME in the cache area (not /Space,
    // which is for real user accounts). Ensure it exists for cargo/git/etc.
    let _ = std::fs::create_dir_all("/Vault/Cache/builder");

    let argv: Vec<CString> = cmd.iter().map(|s| CString::new(*s).unwrap()).collect();

    let term = std::env::var("TERM").unwrap_or("xterm".into());
    let path = if host_links {
        "/Core/Bin:/Construct/Bin:/usr/bin:/bin"
    } else {
        "/Core/Bin:/Construct/Bin"
    };
    // Defaults, then caller envs override by key (glibc getenv returns the first
    // match, so duplicates would not override - dedup explicitly).
    let mut pairs: Vec<(String, String)> = vec![
        ("HOME".to_string(), "/Vault/Cache/builder".to_string()),
        ("TERM".to_string(), term),
        ("PATH".to_string(), path.to_string()),
        // Autotools picks up the RunixOS install layout (StoreRoom/LibKit).
        ("CONFIG_SITE".to_string(), "/Transit/Ephemeral/config.site".to_string()),
    ];
    for (k, v) in envs {
        match pairs.iter_mut().find(|(ek, _)| ek == k) {
            Some(e) => e.1 = v.to_string(),
            None => pairs.push((k.to_string(), v.to_string())),
        }
    }
    let env_strings: Vec<String> = pairs.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
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
    enter_sandbox(sysroot, cmd, envs, host_links, interactive, binds, None)
}

/// Run an interactive session in the sandbox as a specific account (inside
/// uid/gid), so it truly runs as that user. Used for the post-OOBE login.
pub fn run_in_sandbox_as(
    sysroot: &Path,
    cmd: &[&str],
    envs: &[(&str, &str)],
    host_links: bool,
    binds: &[(std::path::PathBuf, String)],
    uid: u32,
    gid: u32,
) -> Result<i32, String> {
    enter_sandbox(
        sysroot,
        cmd,
        envs,
        host_links,
        true,
        binds,
        Some((uid, gid)),
    )
}

/// True when the account store has no human (uid >= 1000) account yet. Read from
/// the plaintext passwd projection so no decryption key is needed.
fn needs_oobe(sysroot: &Path) -> bool {
    match std::fs::read_to_string(sysroot.join("Vault/Accounts/passwd")) {
        Ok(content) => !content.lines().any(|l| {
            l.split(':')
                .nth(2)
                .and_then(|u| u.parse::<u32>().ok())
                .is_some_and(|uid| uid >= 1000)
        }),
        Err(_) => true,
    }
}

/// First human account from the passwd projection: (name, uid, gid, home, shell).
fn session_user(sysroot: &Path) -> Option<(String, u32, u32, String, String)> {
    let content = std::fs::read_to_string(sysroot.join("Vault/Accounts/passwd")).ok()?;
    for line in content.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 7 {
            if let Ok(uid) = f[2].parse::<u32>() {
                if uid >= 1000 {
                    let gid = f[3].parse::<u32>().unwrap_or(uid);
                    return Some((
                        f[0].to_string(),
                        uid,
                        gid,
                        f[5].to_string(),
                        f[6].to_string(),
                    ));
                }
            }
        }
    }
    None
}

/// Enter an interactive session in the sandbox. This stands in for Rev's session
/// role: it runs OOBE if no account exists yet (setup only), then starts a login
/// shell with the user's session environment ($USER, $HOME, $SHELL, ...). The
/// real per-user privilege drop happens in Rev / a login manager; the single-uid
/// build sandbox cannot switch uid, so here it only sets the environment.
pub fn enter_interactive(sysroot: &Path, host_links: bool) -> Result<(), String> {
    if let Ok(release) = std::fs::read_to_string(sysroot.join("Core/Config/OSReleaseInfo")) {
        for line in release.lines() {
            if let Some(name) = line.strip_prefix("PRETTY_NAME=") {
                println!("  Welcome to {}", name.trim_matches('"'));
                break;
            }
        }
    }

    // Out-of-box setup if there is no human account yet. oobe only creates the
    // account; run it forked so control returns here for the actual session.
    let oobe = sysroot.join("Core/Bin/oobe");
    if needs_oobe(sysroot) && oobe.exists() {
        println!("  No user account found - starting setup.");
        run_in_sandbox(sysroot, &["/Core/Bin/oobe"], &[], host_links, false, &[])?;
    }

    // Session: log in as the human account (environment + their login shell),
    // running as that account's uid/gid (not root).
    let session = session_user(sysroot);
    let user_shell = session
        .as_ref()
        .map(|(_, _, _, _, s)| s.clone())
        .filter(|s| s.starts_with('/') && sysroot.join(s.trim_start_matches('/')).exists());

    let (shell, shell_args): (String, Vec<&str>) = if let Some(s) = user_shell {
        let args = if s.ends_with("/nu") {
            vec![]
        } else {
            vec!["--login", "-i"]
        };
        (s, args)
    } else if sysroot.join("Core/Bin/brush").exists() {
        ("/Core/Bin/brush".to_string(), vec!["--login", "-i"])
    } else if sysroot.join("Core/Bin/nu").exists() {
        ("/Core/Bin/nu".to_string(), vec![])
    } else if host_links {
        ("/bin/bash".to_string(), vec!["-i"])
    } else {
        return Err(
            "No RunixOS shell found (brush or nu). Use --enable-host-links for a host fallback."
                .into(),
        );
    };

    let mut owned: Vec<(String, String)> = Vec::new();
    if let Some((name, _, _, home, sh)) = &session {
        owned.push(("USER".into(), name.clone()));
        owned.push(("LOGNAME".into(), name.clone()));
        owned.push(("HOME".into(), home.clone()));
        owned.push(("SHELL".into(), sh.clone()));
    }
    let envs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut cmd: Vec<&str> = vec![shell.as_str()];
    cmd.extend(shell_args.iter().copied());
    // Run the session as the account (so whoami, $HOME ownership, etc. are the
    // user). Without a human account (host-links fallback), stay inside-root.
    let code = match &session {
        Some((_, uid, gid, _, _)) => {
            run_in_sandbox_as(sysroot, &cmd, &envs, host_links, &[], *uid, *gid)?
        }
        None => run_in_sandbox(sysroot, &cmd, &envs, host_links, true, &[])?,
    };
    if code != 0 {
        Err(format!("Shell exited with code {}", code))
    } else {
        Ok(())
    }
}
