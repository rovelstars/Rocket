# Rocket

Rocket is the build system for **RunixOS** - a Rust CLI that builds RunixOS and
its packages. The name comes from the Universe System: each package is a
"planet" and Rocket is the tool that carries them.

Rocket builds RunixOS **from any Linux host** (to bootstrap it the first time)
and **on RunixOS itself** (RunixOS is self-hosting - it rebuilds its own
toolchain and userspace with zero non-RunixOS tools). It targets RunixOS only;
it is not a general-purpose, build-anywhere-for-anything system.

## How it works

Rocket builds each package in an isolated sandbox using unprivileged Linux
namespaces (user + mount) and `chroot` into the RunixOS sysroot - **no Docker,
no daemon, no root required**. The kernel tears the namespace (and its mounts)
down when the build exits, so it is crash-safe and never touches the host.

Two build modes:

1. **Cross-bootstrap** (default): the host's tools (clang, make, cmake, curl,
   ...) are bind-mounted read-only to drive the build, while the RunixOS cross
   toolchain in the sysroot (`/Core/Bin/clang --target=...-runixos`) emits
   RunixOS code. Used to bring up a RunixOS sysroot from a foreign Linux host.
2. **Self-hosted** (`--self-hosted`): no host tools are bound in at all - the
   build uses only the RunixOS-native toolchain and build environment already in
   the sysroot. This is how RunixOS rebuilds itself, and the hermetic path for
   reproducible builds.

## Package structure

Packages live in a separate repository ([Planets](https://github.com/rovelstars/Planets)).
Each package has:

```
package_name/
  meta.toml    # package metadata
  build.sh     # configure/build/install shell functions
  patches/     # (optional) patch files
```

Rocket sources `build.sh` and calls `configure()`, `build()`, `install()` in
order. It provides `$SYSROOT`, `$OUTPUT`, `$SRC`, `$VERSION`, `$REPOSITORY`,
`$BRANCH`, `$JOBS`, and any extra `meta.toml` fields as uppercase env vars.

## Usage

```sh
# Build one package (and install it into the sysroot for later packages)
rocket build <package> --install

# Build it with zero host tools (RunixOS self-hosting / hermetic)
rocket build <package> --self-hosted --install

# Build every package in dependency order
rocket build-all

# Print the resolved dependency build order
rocket deps

# Enter the RunixOS sandbox interactively (--enable-host-links for host tools)
rocket enter

# List available packages
rocket list
```

Defaults: `--planets ../Planets`, `--output ./output`, `--sysroot /home/ren/ROS`.

## Target

RunixOS uses the triple `x86_64-rovelstars-linux-runixos` (glibc + Linux ABI;
RunixOS identity via the `rovelstars` vendor and `runixos` environment). The
toolchain is all-LLVM: clang, lld, and the LLVM binutils - no gcc, no GNU
binutils.

For C packages, build scripts compile with
`$SYSROOT/Core/Bin/clang --target=x86_64-rovelstars-linux-runixos --sysroot=$SYSROOT`.
Rust packages `cargo build --target x86_64-rovelstars-linux-runixos` against the
RunixOS Rust toolchain in the sysroot.

## Pros

- **No root, no Docker, no daemon.** Unprivileged user+mount namespaces; the
  kernel tears the sandbox (and its mounts) down on exit, so builds are
  crash-safe and never mutate the host.
- **One tool, two honest modes.** Cross-bootstrap from any Linux host, and a
  hermetic `--self-hosted` mode that proves RunixOS rebuilds itself with zero
  foreign tools.
- **Simple package model.** A `meta.toml` + a `build.sh` with three functions;
  dependency build order resolved automatically; local-fork override for
  in-development sources.
- All-LLVM, single target triple -- no gcc/GNU-binutils ambiguity.

## Cons / tradeoffs

- **RunixOS-only by design** -- not a general-purpose, build-anything system.
- `build.sh` recipes are shell, not a declarative/sandboxed DSL -- expressive but
  easy to write host-leaking or non-reproducible steps.
- Namespace approach is **Linux-only** and depends on unprivileged user
  namespaces being enabled on the host.

## Known issues / limitations

- **No package format yet** -- Rocket installs build output into the sysroot /
  output dir; there are no shippable, signed package archives or a binary cache,
  so every build is from source.
- Dependency resolution is basic (order from `dependencies`); no version
  constraints, no parallel multi-package builds.
- Defaults are developer-centric (e.g. a hardcoded default `--sysroot`).
- A solid MVP, not yet best-in-class: see the backlog for native/cross-mode
  ergonomics and packaging gaps.
