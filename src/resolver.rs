//! Dependency resolution: order packages so every dependency is built before
//! the packages that depend on it.
//!
//! `meta.toml` carries a `dependencies` list (package names). This turns that
//! into a build order via depth-first topological sort, with cycle detection
//! and validation that every named dependency actually exists. Ordering among
//! otherwise-independent packages is deterministic (names visited sorted) so
//! `build-all` is reproducible.
//!
//! Only build-time package dependencies belong in the graph. External tools
//! (cmake, the kernel headers from KernelFactory, the Rust toolchain) are not
//! packages and must not appear in any `dependencies` list.

use crate::config::Package;
use std::collections::BTreeMap;

/// Topologically order packages (dependencies first).
///
/// `targets = None` orders every package; `Some(names)` restricts the result to
/// those packages plus their transitive dependencies. Errors on an unknown
/// dependency/target or a dependency cycle.
pub fn resolve_order(pkgs: &[Package], targets: Option<&[String]>) -> Result<Vec<String>, String> {
    let deps: BTreeMap<&str, Vec<&str>> = pkgs
        .iter()
        .map(|p| {
            (
                p.meta.name.as_str(),
                p.meta.dependencies.iter().map(String::as_str).collect(),
            )
        })
        .collect();

    // Every declared dependency must be a known package.
    for (name, ds) in &deps {
        for d in ds {
            if !deps.contains_key(d) {
                return Err(format!(
                    "package '{name}' depends on unknown package '{d}'"
                ));
            }
        }
    }

    // Roots: the requested targets, or everything.
    let mut roots: Vec<&str> = match targets {
        Some(ts) => {
            for t in ts {
                if !deps.contains_key(t.as_str()) {
                    return Err(format!("unknown package '{t}'"));
                }
            }
            ts.iter().map(String::as_str).collect()
        }
        None => deps.keys().copied().collect(),
    };
    roots.sort_unstable();
    roots.dedup();

    let mut order = Vec::new();
    let mut state: BTreeMap<&str, Mark> = BTreeMap::new();
    let mut stack: Vec<&str> = Vec::new();
    for r in roots {
        visit(r, &deps, &mut state, &mut order, &mut stack)?;
    }
    Ok(order)
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Visiting,
    Done,
}

fn visit<'a>(
    node: &'a str,
    deps: &BTreeMap<&'a str, Vec<&'a str>>,
    state: &mut BTreeMap<&'a str, Mark>,
    order: &mut Vec<String>,
    stack: &mut Vec<&'a str>,
) -> Result<(), String> {
    match state.get(node) {
        Some(Mark::Done) => return Ok(()),
        Some(Mark::Visiting) => {
            let cycle: Vec<&str> = stack
                .iter()
                .skip_while(|x| **x != node)
                .copied()
                .chain(std::iter::once(node))
                .collect();
            return Err(format!("dependency cycle: {}", cycle.join(" -> ")));
        }
        None => {}
    }

    state.insert(node, Mark::Visiting);
    stack.push(node);

    // Visit dependencies in sorted order for a stable result.
    let mut children = deps[node].clone();
    children.sort_unstable();
    for child in children {
        visit(child, deps, state, order, stack)?;
    }

    stack.pop();
    state.insert(node, Mark::Done);
    order.push(node.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Package, PackageMeta};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn pkg(name: &str, deps: &[&str]) -> Package {
        Package {
            meta: PackageMeta {
                name: name.to_string(),
                version: "0".into(),
                description: String::new(),
                licenses: vec![],
                repository: String::new(),
                dependencies: deps.iter().map(|s| s.to_string()).collect(),
                extra: HashMap::new(),
            },
            build_script: PathBuf::new(),
            patches_dir: None,
            pkg_dir: PathBuf::new(),
        }
    }

    fn idx(order: &[String], name: &str) -> usize {
        order.iter().position(|n| n == name).unwrap()
    }

    #[test]
    fn orders_deps_before_dependents() {
        let pkgs = [
            pkg("curl", &["openssl", "zlib"]),
            pkg("openssl", &["zlib"]),
            pkg("zlib", &[]),
        ];
        let order = resolve_order(&pkgs, None).unwrap();
        assert!(idx(&order, "zlib") < idx(&order, "openssl"));
        assert!(idx(&order, "openssl") < idx(&order, "curl"));
    }

    #[test]
    fn target_closure_only() {
        let pkgs = [
            pkg("curl", &["zlib"]),
            pkg("zlib", &[]),
            pkg("helix", &[]),
        ];
        let order = resolve_order(&pkgs, Some(&["curl".to_string()])).unwrap();
        assert_eq!(order, vec!["zlib".to_string(), "curl".to_string()]);
    }

    #[test]
    fn detects_cycle() {
        let pkgs = [pkg("a", &["b"]), pkg("b", &["a"])];
        let err = resolve_order(&pkgs, None).unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn unknown_dep_errors() {
        let pkgs = [pkg("a", &["ghost"])];
        let err = resolve_order(&pkgs, None).unwrap_err();
        assert!(err.contains("unknown package 'ghost'"), "got: {err}");
    }

    #[test]
    fn deterministic() {
        let pkgs = [pkg("a", &[]), pkg("b", &[]), pkg("c", &[])];
        let o1 = resolve_order(&pkgs, None).unwrap();
        let o2 = resolve_order(&pkgs, None).unwrap();
        assert_eq!(o1, o2);
    }
}
