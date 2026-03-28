# Rocket

Rocket is a Rust based CLI tool for building packages for RunixOS.
The name "Rocket" is inspired by the whole Universe System, where each planet is a package and the rocket is the tool that helps us manage them.

## How it works

Rocket uses Linux namespaces to create isolated build sandboxes, no Docker required. Builds can run in two modes:

1. **Root mode** (recommended): Uses `chroot` with mount/PID namespaces for full isolation. Mount changes are private to the sandbox and automatically cleaned up by the kernel when the process exits.
2. **Non-root mode**: Sets up environment variables and paths so tools find the RunixOS sysroot. Supports cross-compilation from a regular Linux host.

## Package structure

Packages are defined in a separate repository ([Planets](https://github.com/rovelstars/Planets)). Each package has:

```
package_name/
  meta.toml    # package metadata
  build.sh     # build script with configure/build/install functions
  patches/     # (optional) patch files
```

The `build.sh` script defines three shell functions: `configure()`, `build()`, and `install()`. Rocket sources the script and calls each function in order. Environment variables like `$SYSROOT`, `$OUTPUT`, `$SRC`, `$VERSION`, `$REPOSITORY`, and `$JOBS` are provided automatically.

## Usage

```sh
# Build a package
rocket build <package> --planets <path-to-planets> --output <output-dir> --sysroot <sysroot-path>

# Build all packages
rocket build-all --planets <path-to-planets> --output <output-dir> --sysroot <sysroot-path>

# Enter the RunixOS sandbox interactively
rocket enter --sysroot <sysroot-path>

# Enter with host tools available (for debugging)
rocket enter --sysroot <sysroot-path> --enable-host-links

# List available packages
rocket list --planets <path-to-planets>
```

Root mode is used automatically when run with `sudo`. Otherwise, non-root mode is used with cross-compilation environment variables.

## Cross-compilation

Rocket sets up the cross-compilation environment for RunixOS (`x86_64-rovelstars-runixos`) automatically. For Rust packages, it configures `RUSTC`, `CARGO_TARGET_*_RUSTFLAGS`, `CC`, `CXX`, and `CFLAGS` so that `cargo build --target x86_64-rovelstars-runixos` works out of the box.

For C packages, build scripts should use `$SYSROOT/Core/Bin/clang` with `--target=x86_64-rovelstars-runixos --sysroot=$SYSROOT` flags.
