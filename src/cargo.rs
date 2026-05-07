use std::collections::{HashMap, HashSet};

use crate::core::{config_extra_keys, BuildConfigKey, Config, Context, ResolverPlugin};
use crate::plugins::plugin_kind;

#[derive(Debug)]
pub struct CargoResolver {
    locked_dependencies: HashMap<String, HashMap<String, String>>,
    build_recipes: HashMap<String, CargoBuildRecipe>,
}

#[derive(Debug, Clone, Default)]
pub struct CargoBuildRecipe {
    pub rustc_cfgs: Vec<String>,
}

impl CargoResolver {
    pub fn new() -> Self {
        Self {
            locked_dependencies: HashMap::new(),
            build_recipes: HashMap::new(),
        }
    }

    pub fn with_build_recipes<I, S>(mut self, recipes: I) -> Self
    where
        I: IntoIterator<Item = (S, CargoBuildRecipe)>,
        S: Into<String>,
    {
        self.build_recipes.extend(
            recipes
                .into_iter()
                .map(|(target, recipe)| (target.into(), recipe)),
        );
        self
    }

    pub fn with_locked_dependencies<I, S, J, K, V>(mut self, dependencies: I) -> Self
    where
        I: IntoIterator<Item = (S, J)>,
        S: Into<String>,
        J: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.locked_dependencies
            .extend(dependencies.into_iter().map(|(target, deps)| {
                (
                    target.into(),
                    deps.into_iter()
                        .map(|(package, target)| (package.into(), target.into()))
                        .collect(),
                )
            }));
        self
    }

    pub fn from_cargo_lock<P: AsRef<std::path::Path>>(
        lockfile: P,
    ) -> std::io::Result<(Self, HashMap<String, String>)> {
        let content = std::fs::read_to_string(lockfile)?;
        let table = content
            .parse::<toml::Table>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let packages = table
            .get("package")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Cargo.lock does not contain packages",
                )
            })?;

        let mut package_counts: HashMap<String, usize> = HashMap::new();
        let mut parsed_packages = Vec::new();
        for package in packages {
            let package = package.as_table().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid package entry")
            })?;
            let name = package
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "package missing name")
                })?
                .to_string();
            let version = package
                .get("version")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "package missing version")
                })?
                .to_string();
            let dependencies = package
                .get("dependencies")
                .and_then(|v| v.as_array())
                .map(|deps| {
                    deps.iter()
                        .filter_map(|dep| dep.as_str().map(|dep| dep.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            *package_counts.entry(name.clone()).or_default() += 1;
            parsed_packages.push(LockedPackage {
                name,
                version,
                dependencies,
            });
        }

        let mut package_targets: HashMap<(String, String), String> = HashMap::new();
        let mut lock_entries = HashMap::new();
        for package in &parsed_packages {
            let target = cargo_target_name(&package.name, &package.version, &package_counts);
            package_targets.insert(
                (package.name.clone(), package.version.clone()),
                target.clone(),
            );
            lock_entries.insert(target.clone(), package.version.clone());

            let versioned_target = format!("cargo://{}@{}", package.name, package.version);
            lock_entries.insert(versioned_target, package.version.clone());
        }

        let mut locked_dependencies = HashMap::new();
        for package in &parsed_packages {
            let mut deps = HashMap::new();
            for dep in &package.dependencies {
                let (dep_name, dep_version) = parse_lock_dependency(dep);
                let dep_target = match dep_version {
                    Some(version) => package_targets
                        .get(&(dep_name.to_string(), version.to_string()))
                        .cloned(),
                    None => parsed_packages
                        .iter()
                        .filter(|p| p.name == dep_name)
                        .map(|p| (p.name.clone(), p.version.clone()))
                        .next()
                        .and_then(|key| package_targets.get(&key).cloned()),
                };
                if let Some(dep_target) = dep_target {
                    deps.insert(dep_name.to_string(), dep_target);
                }
            }

            let target = cargo_target_name(&package.name, &package.version, &package_counts);
            locked_dependencies.insert(target.clone(), deps.clone());
            locked_dependencies.insert(
                format!("cargo://{}@{}", package.name, package.version),
                deps,
            );
        }

        Ok((
            Self {
                locked_dependencies,
                build_recipes: HashMap::new(),
            },
            lock_entries,
        ))
    }
}

fn get_rust_files(
    path: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

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
    dependencies: Vec<CargoDependency>,
    crate_name: String,
    crate_type: String,
    edition: String,
    root_source: std::path::PathBuf,
    features: Vec<String>,
    has_build_script: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoDependency {
    alias: String,
    package: String,
}

#[derive(Debug)]
struct DependencySpec {
    alias: String,
    package: String,
    optional: bool,
}

#[derive(Debug)]
struct LockedPackage {
    name: String,
    version: String,
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

    let manifest_dir = filename.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("manifest has no parent directory: {}", filename.display()),
        )
    })?;

    let package = table
        .get("package")
        .and_then(|v| v.as_table())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Cargo.toml missing [package]",
            )
        })?;
    let package_name = package
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Cargo.toml package missing name",
            )
        })?;
    let edition = package
        .get("edition")
        .and_then(|v| v.as_str())
        .unwrap_or("2015")
        .to_string();
    let has_build_script = match package.get("build") {
        Some(toml::Value::Boolean(false)) => false,
        Some(toml::Value::String(_)) => true,
        Some(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Cargo.toml package build key must be a string or false",
            ))
        }
        None => manifest_dir.join("build.rs").exists(),
    };

    let lib = table.get("lib").and_then(|v| v.as_table());
    let crate_name = lib
        .and_then(|t| t.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or(package_name)
        .replace('-', "_");
    let crate_type = if lib
        .and_then(|t| t.get("proc-macro"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        "proc-macro".to_string()
    } else {
        "rlib".to_string()
    };
    let root_source = manifest_dir.join(
        lib.and_then(|t| t.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("src/lib.rs"),
    );

    let mut dependency_specs = Vec::new();
    if let Some(toml::Value::Table(deps_table)) = table.get("dependencies") {
        for (k, v) in deps_table {
            dependency_specs.push(parse_dependency_spec(k, v));
        }
    }
    if let Some(toml::Value::Table(targets)) = table.get("target") {
        for (target, target_table) in targets {
            let include = if target.starts_with("cfg(") && target.ends_with(')') {
                resolve_cfg_directive(context, &target[4..target.len() - 1])?
            } else {
                false
            };
            if !include {
                continue;
            }

            if let Some(toml::Value::Table(deps_table)) = target_table.get("dependencies") {
                for (k, v) in deps_table {
                    dependency_specs.push(parse_dependency_spec(k, v));
                }
            }
        }
    }

    let mut features_table = HashMap::new();
    if let Some(toml::Value::Table(t)) = table.get("features") {
        for (k, v) in t {
            let members = v
                .as_array()
                .map(|deps| {
                    deps.iter()
                        .filter_map(|dep| dep.as_str().map(|dep| dep.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            features_table.insert(k.to_string(), members);
        }
    }

    let enabled_features = expand_features(&features_table, features);
    let mut optional_deps = HashSet::new();
    let optional_aliases: HashSet<_> = dependency_specs
        .iter()
        .filter(|dep| dep.optional)
        .map(|dep| dep.alias.as_str())
        .collect();
    for feature in &enabled_features {
        if optional_aliases.contains(feature.as_str()) {
            optional_deps.insert(feature.to_string());
        }
    }
    for feature in &enabled_features {
        let Some(members) = features_table.get(feature) else {
            continue;
        };
        for member in members {
            if let Some(dep) = member.strip_prefix("dep:") {
                optional_deps.insert(dep.to_string());
                continue;
            }

            if let Some((dep, _)) = member.split_once('/') {
                let dep = dep.trim_end_matches('?');
                if optional_aliases.contains(dep) && !member.contains("?/") {
                    optional_deps.insert(dep.to_string());
                }
                continue;
            }

            if !features_table.contains_key(member) && optional_aliases.contains(member.as_str()) {
                optional_deps.insert(member.to_string());
            }
        }
    }

    let mut seen = HashSet::new();
    let mut dependencies = Vec::new();
    for dep in dependency_specs {
        if dep.optional && !optional_deps.contains(&dep.alias) {
            continue;
        }
        if seen.insert(dep.alias.clone()) {
            dependencies.push(CargoDependency {
                alias: dep.alias,
                package: dep.package,
            });
        }
    }

    let mut features: Vec<_> = enabled_features.into_iter().collect();
    features.sort();

    Ok(CargoToml {
        dependencies,
        crate_name,
        crate_type,
        edition,
        root_source,
        features,
        has_build_script,
    })
}

fn resolve_cfg_directive(context: &Context, directive: &str) -> std::io::Result<bool> {
    let directive = directive.trim();
    if let Some(args) = strip_cfg_call(directive, "all") {
        for arg in split_cfg_args(args)? {
            if !resolve_cfg_directive(context, arg)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if let Some(args) = strip_cfg_call(directive, "any") {
        for arg in split_cfg_args(args)? {
            if resolve_cfg_directive(context, arg)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if let Some(args) = strip_cfg_call(directive, "not") {
        let args = split_cfg_args(args)?;
        if args.len() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("not() expects one cfg argument: {directive}"),
            ));
        }
        return Ok(!resolve_cfg_directive(context, args[0])?);
    }

    if directive == "unix" {
        return Ok(context.get_config(BuildConfigKey::TargetFamily) == Some("unix"));
    }
    if directive == "windows" {
        return Ok(context.get_config(BuildConfigKey::TargetFamily) == Some("windows"));
    }

    if let Some((key, value)) = directive.split_once('=') {
        let value = value.trim().trim_matches('"');
        return Ok(match key.trim() {
            "target_family" => context.get_config(BuildConfigKey::TargetFamily) == Some(value),
            "target_os" => context.get_config(BuildConfigKey::TargetOS) == Some(value),
            "target_env" => context.get_config(BuildConfigKey::TargetEnv) == Some(value),
            _ => false,
        });
    }

    Ok(false)
}

fn parse_dependency_spec(alias: &str, value: &toml::Value) -> DependencySpec {
    let (package, optional) = match value.as_table() {
        Some(table) => (
            table
                .get("package")
                .and_then(|v| v.as_str())
                .unwrap_or(alias)
                .to_string(),
            table
                .get("optional")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
        None => (alias.to_string(), false),
    };

    DependencySpec {
        alias: alias.to_string(),
        package,
        optional,
    }
}

fn expand_features(
    features_table: &HashMap<String, Vec<String>>,
    requested_features: &[&str],
) -> HashSet<String> {
    let mut enabled = HashSet::new();
    let mut stack: Vec<String> = if requested_features.is_empty() {
        if features_table.contains_key("default") {
            vec!["default".to_string()]
        } else {
            Vec::new()
        }
    } else {
        requested_features
            .iter()
            .filter(|feature| !feature.is_empty())
            .map(|feature| feature.to_string())
            .collect()
    };

    while let Some(feature) = stack.pop() {
        if !enabled.insert(feature.clone()) {
            continue;
        }

        let Some(members) = features_table.get(&feature) else {
            continue;
        };
        for member in members {
            if member.starts_with("dep:") || member.contains('/') {
                continue;
            }
            if features_table.contains_key(member) {
                stack.push(member.to_string());
            }
        }
    }

    enabled
}

fn strip_cfg_call<'a>(directive: &'a str, name: &str) -> Option<&'a str> {
    directive
        .strip_prefix(name)
        .and_then(|rest| rest.trim_start().strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
}

fn split_cfg_args(args: &str) -> std::io::Result<Vec<&str>> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut start = 0usize;
    for (idx, ch) in args.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unbalanced cfg directive: {args}"),
                    )
                })?;
            }
            ',' if !in_string && depth == 0 => {
                out.push(args[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    if in_string || depth != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unbalanced cfg directive: {args}"),
        ));
    }
    let tail = args[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    Ok(out)
}

fn parse_cargo_target(target: &str) -> std::io::Result<(&str, Option<&str>)> {
    let crate_name = target
        .strip_prefix("cargo://")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "invalid target name"))?;
    Ok(match crate_name.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (crate_name, None),
    })
}

fn parse_lock_dependency(dependency: &str) -> (&str, Option<&str>) {
    let mut parts = dependency.split_whitespace();
    let name = parts.next().unwrap_or(dependency);
    let version = parts.next();
    (name, version)
}

fn cargo_target_name(
    package_name: &str,
    package_version: &str,
    package_counts: &HashMap<String, usize>,
) -> String {
    if package_counts.get(package_name).copied().unwrap_or(0) > 1 {
        format!("cargo://{package_name}@{package_version}")
    } else {
        format!("cargo://{package_name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cbs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_parse_manifest_features_aliases_and_target_cfgs() {
        let dir = temp_dir("manifest");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/custom.rs"), "").unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            r#"
[package]
name = "demo-crate"
version = "1.0.0"
edition = "2021"

[lib]
name = "demo_lib"
path = "src/custom.rs"
proc-macro = true

[dependencies]
bytes = "1"
serde_alias = { package = "serde", version = "1", optional = true }

[target.'cfg(all(unix, target_os = "linux", not(target_env = "musl")))'.dependencies]
libc = { version = "0.2", optional = true }

[target.'cfg(windows)'.dependencies]
winapi = "0.3"

[features]
default = ["std"]
std = ["serde_alias", "dep:libc"]
"#,
        )
        .unwrap();

        let context = Context::new(
            std::env::temp_dir(),
            [
                (BuildConfigKey::TargetFamily, "unix".to_string()),
                (BuildConfigKey::TargetOS, "linux".to_string()),
                (BuildConfigKey::TargetEnv, "gnu".to_string()),
            ],
        );

        let manifest = parse_cargo_toml(&context, &dir.join("Cargo.toml"), &[]).unwrap();
        assert_eq!(manifest.crate_name, "demo_lib");
        assert_eq!(manifest.crate_type, "proc-macro");
        assert_eq!(manifest.edition, "2021");
        assert_eq!(manifest.root_source, dir.join("src/custom.rs"));
        assert!(!manifest.has_build_script);
        assert_eq!(
            manifest.features,
            vec!["default".to_string(), "std".to_string()]
        );
        assert_eq!(
            manifest.dependencies,
            vec![
                CargoDependency {
                    alias: "bytes".to_string(),
                    package: "bytes".to_string(),
                },
                CargoDependency {
                    alias: "serde_alias".to_string(),
                    package: "serde".to_string(),
                },
                CargoDependency {
                    alias: "libc".to_string(),
                    package: "libc".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_parse_manifest_detects_build_script() {
        let dir = temp_dir("build-script");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("build.rs"), "").unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "1.0.0"
"#,
        )
        .unwrap();

        let context = Context::new(
            std::env::temp_dir(),
            std::iter::empty::<(BuildConfigKey, String)>(),
        );
        let manifest = parse_cargo_toml(&context, &dir.join("Cargo.toml"), &[]).unwrap();
        assert!(manifest.has_build_script);

        std::fs::write(
            dir.join("Cargo.toml"),
            r#"
[package]
name = "demo"
version = "1.0.0"
build = false
"#,
        )
        .unwrap();
        let manifest = parse_cargo_toml(&context, &dir.join("Cargo.toml"), &[]).unwrap();
        assert!(!manifest.has_build_script);
    }

    #[test]
    fn test_cargo_lock_uses_version_qualified_duplicate_targets() {
        let dir = temp_dir("lock");
        let lockfile = dir.join("Cargo.lock");
        std::fs::write(
            &lockfile,
            r#"
version = 3

[[package]]
name = "bytes"
version = "0.5.6"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "bytes"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "hyper"
version = "0.13.10"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "bytes 0.5.6",
]
"#,
        )
        .unwrap();

        let (resolver, lock_entries) = CargoResolver::from_cargo_lock(&lockfile).unwrap();
        assert_eq!(lock_entries.get("cargo://bytes@0.5.6").unwrap(), "0.5.6");
        assert_eq!(lock_entries.get("cargo://bytes@1.0.0").unwrap(), "1.0.0");
        assert_eq!(lock_entries.get("cargo://hyper").unwrap(), "0.13.10");
        assert_eq!(
            resolver
                .locked_dependencies
                .get("cargo://hyper")
                .and_then(|deps| deps.get("bytes")),
            Some(&"cargo://bytes@0.5.6".to_string())
        );
    }
}

impl ResolverPlugin for CargoResolver {
    fn can_resolve(&self, target: &str) -> bool {
        target.starts_with("cargo://")
    }

    fn resolve(&self, context: Context, target: &str) -> std::io::Result<Config> {
        let (crate_name, target_version) = parse_cargo_target(target)?;

        let lockstring = match context.get_locked_version(target) {
            Ok(lockstring) => lockstring,
            Err(e) => match target_version {
                Some(version) => version.to_string(),
                None => return Err(e),
            },
        };
        let lockstring = &lockstring;
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

        let toml = parse_cargo_toml(&context, &dest.join("Cargo.toml"), &features)?;
        let build_recipe = if toml.has_build_script {
            Some(self.build_recipes.get(target).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "{target} declares build.rs, but no hermetic Cargo build recipe was provided"
                    ),
                )
            })?)
        } else {
            None
        };

        let mut deps = Vec::new();
        let mut dependency_aliases = Vec::new();
        for dep in toml.dependencies {
            let dep_target = self
                .locked_dependencies
                .get(target)
                .and_then(|deps| deps.get(&dep.package))
                .cloned()
                .unwrap_or_else(|| format!("cargo://{}", dep.package));
            dependency_aliases.push(format!("{dep_target}={}", dep.alias.replace('-', "_")));
            deps.push(dep_target);
        }

        let mut extras = HashMap::new();
        extras.insert(config_extra_keys::FEATURES, toml.features);
        extras.insert(config_extra_keys::CRATE_NAME, vec![toml.crate_name]);
        extras.insert(config_extra_keys::CRATE_TYPE, vec![toml.crate_type]);
        extras.insert(config_extra_keys::EDITION, vec![toml.edition]);
        extras.insert(
            config_extra_keys::ROOT_SOURCE,
            vec![toml.root_source.to_string_lossy().to_string()],
        );
        extras.insert(config_extra_keys::DEPENDENCY_ALIASES, dependency_aliases);
        extras.insert(
            config_extra_keys::RUSTC_CFGS,
            build_recipe
                .map(|recipe| recipe.rustc_cfgs.clone())
                .unwrap_or_default(),
        );

        Ok(Config {
            dependencies: deps,
            build_plugin: "@rust_plugin".to_string(),
            location: None,
            sources: rust_files
                .into_iter()
                .map(|s| s.to_string_lossy().to_string())
                .collect(),
            build_dependencies: vec!["@rust_compiler".to_string()],
            kind: plugin_kind::RUST_LIBRARY.to_string(),
            extras,
            hash: 1010,
        })
    }
}
