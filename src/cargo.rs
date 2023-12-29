use std::collections::{HashMap, HashSet};

use crate::core::{BuildConfigKey, Config, ConfigExtraKeys, Context, ResolverPlugin};
use crate::plugins::PluginKind;

#[derive(Debug)]
pub struct CargoResolver {}

impl CargoResolver {
    pub fn new() -> Self {
        Self {}
    }
}

fn get_rust_files(
    path: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_symlink() {
            continue;
        }

        if metadata.is_dir() {
            get_rust_files(&entry.path(), out)?;
        }

        if let Some(ext) = entry.path().extension() {
            if ext == "rs" {
                out.push(entry.path());
            }
        }
    }
    Ok(())
}

fn parse_lockstring(l: &str) -> (&str, Vec<&str>) {
    let mut components = l.split(",");
    let version = components.next().expect("always get at least one split");
    let features = components.collect();
    (version, features)
}

#[derive(Debug)]
struct CargoToml {
    dependencies: Vec<String>,
}

fn parse_cargo_toml(
    context: &Context,
    filename: &std::path::Path,
    features: &[&str],
) -> std::io::Result<CargoToml> {
    let content = std::fs::read_to_string(filename)?;
    let table = content
        .parse::<toml::Table>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let targets = table.get("target");
    let target_deps_iter = targets
        .iter()
        .filter_map(|v| {
            if let toml::Value::Table(t) = v {
                return Some(t);
            }
            None
        })
        .flatten()
        .filter_map(|(k, v)| {
            if k.starts_with("cfg(") {
                if !resolve_cfg_directive(context, &k[4..k.len() - 1]).ok()? {
                    return None;
                }
            }

            if let toml::Value::Table(t) = v {
                if let Some(toml::Value::Table(t)) = v.get("dependencies") {
                    return Some(t);
                }
            }
            None
        })
        .flatten();
    let deps_table = table.get("dependencies");
    let deps_table_iter = deps_table
        .iter()
        .filter_map(|v| {
            if let toml::Value::Table(t) = v {
                return Some(t);
            }
            None
        })
        .flatten()
        .chain(target_deps_iter);

    let mut dependencies = Vec::new();
    for (k, v) in deps_table_iter {
        // Exclude optional dependencies
        if let toml::Value::Table(t) = v {
            if matches!(v.get("optional"), Some(toml::Value::Boolean(true))) {
                continue;
            }
        }

        dependencies.push(k.to_string());
    }

    let mut all_features = HashSet::new();
    if let Some(toml::Value::Table(t)) = table.get("features") {
        for (k, v) in t {
            all_features.insert(k);
        }
    }

    let mut optional_deps = HashSet::new();
    if let Some(toml::Value::Table(t)) = table.get("features") {
        for (k, v) in t {
            if features.iter().find(|fname| k == *fname).is_none() {
                continue;
            }
            if let toml::Value::Array(deps) = v {
                for dep in deps {
                    if let toml::Value::String(dname) = dep {
                        // dep: prefix allows resolution of dependencies with the same name
                        // as features.
                        if dname.starts_with("dep:") {
                            optional_deps.insert(dname[4..].to_string());
                            continue;
                        }

                        // If a feature exists with this name, it represents a feature constraint
                        // and not a dependency constraint.
                        if all_features.contains(&dname) {
                            continue;
                        }

                        // If it contains a slash, it's a dependency feature constraint.
                        if dname.contains('/') {
                            continue;
                        }

                        // Must be a dependency
                        optional_deps.insert(dname.to_string());
                    }
                }
            }
        }
    }

    dependencies.extend(optional_deps.into_iter());

    Ok(CargoToml { dependencies })
}

// TODO: properly implement this (need to actually parse the cfg directive...)
fn resolve_cfg_directive(context: &Context, directive: &str) -> std::io::Result<bool> {
    if directive == "unix" && context.get_config(BuildConfigKey::TargetFamily) == Some("unix") {
        return Ok(true);
    }
    Ok(false)
}

impl ResolverPlugin for CargoResolver {
    fn can_resolve(&self, target: &str) -> bool {
        target.starts_with("cargo://")
    }

    fn resolve(&self, context: Context, target: &str) -> std::io::Result<Config> {
        let crate_name = target.strip_prefix("cargo://").ok_or(std::io::Error::new(
            std::io::ErrorKind::Other,
            "invalid target name",
        ))?;

        let lockstring = &context.get_locked_version(target)?;
        let (crate_version, features) = parse_lockstring(&lockstring);

        let workdir = context.working_directory();
        std::fs::create_dir_all(&workdir).ok();

        // Download the crate tarball
        let tar_dest = workdir.join("crate.tar");

        if !tar_dest.exists() {
            context.actions.download(
                &context,
                format!(
                    "https://crates.io/api/v1/crates/{}/{}/download",
                    crate_name, crate_version
                ),
                &tar_dest,
            )?;
        }

        // Untar the crate tarball
        let dest = workdir.join("crate");
        if !dest.exists() {
            std::fs::create_dir_all(&dest).ok();
            context.actions.run_process(
                &context,
                "tar",
                &[
                    "xzvf",
                    &tar_dest.to_string_lossy(),
                    "-C",
                    &dest.to_string_lossy(),
                    "--strip-components=1",
                ],
            )?;
        }

        let mut rust_files = Vec::new();
        get_rust_files(&dest.join("src"), &mut rust_files)?;

        let mut extras = HashMap::new();
        extras.insert(
            ConfigExtraKeys::Features,
            features.iter().map(|s| s.to_string()).collect(),
        );

        let toml = parse_cargo_toml(&context, &dest.join("Cargo.toml"), &features)?;

        let mut deps = Vec::new();
        for dep in toml.dependencies {
            deps.push(format!("cargo://{dep}"));
        }

        Ok(Config {
            dependencies: deps,
            build_plugin: "@rust_plugin".to_string(),
            location: None,
            sources: rust_files
                .into_iter()
                .map(|s| s.to_string_lossy().to_string())
                .collect(),
            build_dependencies: vec!["@rust_compiler".to_string()],
            kind: PluginKind::RustLibrary.to_string(),
            extras,
            hash: 1010,
        })
    }
}
