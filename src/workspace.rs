use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::cargo::{CargoDependencyPlanner, CargoResolver};
use crate::core::{
    config_extra_keys, BuildConfigKey, BuildResult, Config, Context, ExternalRequirement,
    FakeResolver, FilesystemBuilder, ResolverPlugin,
};
use crate::exec::Executor;
use crate::plugins::plugin_kind;

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    current_package: String,
    config: WorkspaceConfig,
}

#[derive(Debug, Clone)]
struct WorkspaceConfig {
    cache_dir: PathBuf,
    rustc: String,
    target_config: Vec<(BuildConfigKey, String)>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceResolver {
    root: PathBuf,
    current_package: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Label {
    package: String,
    name: String,
}

impl Workspace {
    pub fn load_from(cwd: &Path) -> std::io::Result<Self> {
        let root = find_workspace_root(cwd)?;
        let workspace_file = workspace_file(&root).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("workspace file disappeared from {}", root.display()),
            )
        })?;
        let config = load_workspace_config(&root, &workspace_file)?;
        let current_package = package_for_cwd(&root, cwd)?;
        Ok(Self {
            root,
            current_package,
            config,
        })
    }

    pub fn executor(&self) -> Executor {
        let mut context = Context::new(
            self.config.cache_dir.clone(),
            self.config.target_config.clone(),
        );
        context.calculate_hash();

        let mut executor = Executor::with_context(context);
        executor.add_builder_plugin("@filesystem", Arc::new(FilesystemBuilder {}));
        executor.add_resolver_plugin(Box::new(WorkspaceResolver {
            root: self.root.clone(),
            current_package: self.current_package.clone(),
        }));
        executor.add_resolver_plugin(Box::new(CargoResolver::new()));
        executor.add_resolver_plugin(Box::new(FakeResolver::with_configs(vec![
            (
                "@rust_compiler",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some(self.config.rustc.clone()),
                    ..Default::default()
                }),
            ),
            (
                "@rust_plugin",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/tmp/rust.cdylib".to_string()),
                    ..Default::default()
                }),
            ),
        ])));
        executor.add_dependency_planner_plugin(Box::new(CargoDependencyPlanner::new()));
        executor
    }
}

impl ResolverPlugin for WorkspaceResolver {
    fn can_resolve(&self, target: &str) -> bool {
        target.starts_with("//") || target.starts_with(':')
    }

    fn resolve(&self, _context: Context, target: &str) -> std::io::Result<Config> {
        let label = parse_label(target, &self.current_package)?;
        let package_dir = self.root.join(&label.package);
        validate_workspace_relative(&self.root, &package_dir)?;
        let build_file = package_dir.join("BUILD.toml");
        let table = std::fs::read_to_string(&build_file)?
            .parse::<toml::Table>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        for (section, kind) in [
            ("rust_binary", plugin_kind::RUST_BINARY),
            ("rust_library", plugin_kind::RUST_LIBRARY),
        ] {
            if let Some(target_table) = find_named_target(&table, section, &label.name)? {
                return config_from_target(
                    &self.root,
                    &label.package,
                    &package_dir,
                    kind,
                    target_table,
                );
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("target {target} not found in {}", build_file.display()),
        ))
    }
}

pub fn build_from_current_workspace(target: &str) -> std::io::Result<BuildResult> {
    let cwd = std::env::current_dir()?;
    let workspace = Workspace::load_from(&cwd)?;
    let mut executor = workspace.executor();
    let root = executor.add_task(target, None);
    Ok(executor.run(&[root]))
}

fn config_from_target(
    root: &Path,
    package: &str,
    package_dir: &Path,
    kind: &str,
    target: &toml::Table,
) -> std::io::Result<Config> {
    let sources = string_list(target, "srcs")?
        .into_iter()
        .map(|src| package_path(root, package_dir, &src))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect();
    let dependencies = string_list(target, "deps")?
        .into_iter()
        .map(|dep| parse_label(&dep, package).map(|label| canonical_label(&label)))
        .collect::<std::io::Result<Vec<_>>>()?;
    let external_requirements = cargo_requirements(target, package)?;

    let mut extras = HashMap::new();
    if let Some(edition) = target.get("edition").and_then(|value| value.as_str()) {
        extras.insert(config_extra_keys::EDITION, vec![edition.to_string()]);
    }

    Ok(Config {
        dependencies,
        external_requirements,
        build_plugin: "@rust_plugin".to_string(),
        sources,
        build_dependencies: vec!["@rust_compiler".to_string()],
        kind: kind.to_string(),
        extras,
        ..Default::default()
    })
}

fn cargo_requirements(
    target: &toml::Table,
    package: &str,
) -> std::io::Result<Vec<ExternalRequirement>> {
    let Some(value) = target.get("cargo_deps") else {
        return Ok(Vec::new());
    };
    let deps = value.as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cargo_deps must be an array of tables",
        )
    })?;
    deps.iter()
        .map(|dep| {
            let table = dep.as_table().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "cargo_deps entries must be tables",
                )
            })?;
            let package_name = required_string(table, "package")?;
            let version = required_string(table, "version")?;
            Ok(ExternalRequirement {
                ecosystem: "cargo".to_string(),
                package: package_name.clone(),
                version,
                features: string_list(table, "features")?,
                default_features: table
                    .get("default_features")
                    .or_else(|| table.get("default-features"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(true),
                target: table
                    .get("target")
                    .and_then(|value| value.as_str())
                    .map(|target| parse_label_or_external(target, package))
                    .transpose()?
                    .or_else(|| Some(format!("cargo://{package_name}"))),
            })
        })
        .collect()
}

fn find_named_target<'a>(
    table: &'a toml::Table,
    section: &str,
    name: &str,
) -> std::io::Result<Option<&'a toml::Table>> {
    let Some(value) = table.get(section) else {
        return Ok(None);
    };
    let targets = value.as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{section} must be an array of tables"),
        )
    })?;
    for target in targets {
        let target = target.as_table().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{section} entries must be tables"),
            )
        })?;
        if target.get("name").and_then(|value| value.as_str()) == Some(name) {
            return Ok(Some(target));
        }
    }
    Ok(None)
}

fn load_workspace_config(root: &Path, workspace_file: &Path) -> std::io::Result<WorkspaceConfig> {
    let table = std::fs::read_to_string(workspace_file)?
        .parse::<toml::Table>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let rustc = table
        .get("toolchain")
        .and_then(|value| value.as_table())
        .and_then(|toolchain| toolchain.get("rust"))
        .and_then(|value| value.as_table())
        .and_then(|rust| rust.get("rustc"))
        .and_then(|value| value.as_str())
        .map(|rustc| root_relative(root, rustc))
        .unwrap_or_else(|| std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()));
    let cache_dir = table
        .get("workspace")
        .and_then(|value| value.as_table())
        .and_then(|workspace| workspace.get("cache_dir"))
        .and_then(|value| value.as_str())
        .map(|cache_dir| root.join(cache_dir))
        .unwrap_or_else(|| root.join(".cbs").join("cache"));

    Ok(WorkspaceConfig {
        cache_dir,
        rustc,
        target_config: target_config(&table),
    })
}

fn target_config(table: &toml::Table) -> Vec<(BuildConfigKey, String)> {
    let target = table.get("target").and_then(|value| value.as_table());
    let os = target
        .and_then(|target| target.get("os"))
        .and_then(|value| value.as_str())
        .unwrap_or(std::env::consts::OS);
    let family = target
        .and_then(|target| target.get("family"))
        .and_then(|value| value.as_str())
        .unwrap_or(if cfg!(windows) { "windows" } else { "unix" });
    let arch = target
        .and_then(|target| target.get("arch"))
        .and_then(|value| value.as_str())
        .unwrap_or(std::env::consts::ARCH);
    let vendor_default = match os {
        "macos" | "ios" | "tvos" | "visionos" | "watchos" => "apple",
        "windows" => "pc",
        _ => "unknown",
    };
    let env_default = match os {
        "linux" => "gnu",
        "windows" => "msvc",
        _ => "",
    };
    let vendor = target
        .and_then(|target| target.get("vendor"))
        .and_then(|value| value.as_str())
        .unwrap_or(vendor_default);
    let env = target
        .and_then(|target| target.get("env"))
        .and_then(|value| value.as_str())
        .unwrap_or(env_default);
    let endian = target
        .and_then(|target| target.get("endian"))
        .and_then(|value| value.as_str())
        .unwrap_or(if cfg!(target_endian = "little") {
            "little"
        } else {
            "big"
        });

    vec![
        (BuildConfigKey::TargetFamily, family.to_string()),
        (BuildConfigKey::TargetOS, os.to_string()),
        (BuildConfigKey::TargetEnv, env.to_string()),
        (BuildConfigKey::TargetArch, arch.to_string()),
        (BuildConfigKey::TargetVendor, vendor.to_string()),
        (BuildConfigKey::TargetEndian, endian.to_string()),
    ]
}

fn parse_label_or_external(value: &str, current_package: &str) -> std::io::Result<String> {
    if value.starts_with("//") || value.starts_with(':') {
        return parse_label(value, current_package).map(|label| canonical_label(&label));
    }
    Ok(value.to_string())
}

fn parse_label(value: &str, current_package: &str) -> std::io::Result<Label> {
    let (package, name) = if let Some(rest) = value.strip_prefix("//") {
        rest.split_once(':').ok_or_else(|| invalid_label(value))?
    } else if let Some(name) = value.strip_prefix(':') {
        (current_package, name)
    } else {
        return Err(invalid_label(value));
    };
    if name.is_empty() || package.split('/').any(|part| part == "..") || package.starts_with('/') {
        return Err(invalid_label(value));
    }
    Ok(Label {
        package: package.trim_matches('/').to_string(),
        name: name.to_string(),
    })
}

fn canonical_label(label: &Label) -> String {
    if label.package.is_empty() {
        format!("//:{}", label.name)
    } else {
        format!("//{}:{}", label.package, label.name)
    }
}

fn package_path(root: &Path, package_dir: &Path, path: &str) -> std::io::Result<PathBuf> {
    if path.starts_with('/')
        || Path::new(path)
            .components()
            .any(|part| part == Component::ParentDir)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("workspace paths must be package-relative: {path}"),
        ));
    }
    let resolved = package_dir.join(path);
    validate_workspace_relative(root, &resolved)?;
    Ok(resolved)
}

fn validate_workspace_relative(root: &Path, path: &Path) -> std::io::Result<()> {
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path escapes workspace: {}", path.display()),
        ));
    }
    if path.is_absolute() && !path.starts_with(root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path escapes workspace: {}", path.display()),
        ));
    }
    Ok(())
}

fn string_list(table: &toml::Table, key: &str) -> std::io::Result<Vec<String>> {
    let Some(value) = table.get(key) else {
        return Ok(Vec::new());
    };
    let values = value.as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{key} must be an array of strings"),
        )
    })?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(|value| value.to_string())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{key} must be an array of strings"),
                    )
                })
        })
        .collect()
}

fn required_string(table: &toml::Table, key: &str) -> std::io::Result<String> {
    table
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("missing required string field {key}"),
            )
        })
}

fn find_workspace_root(cwd: &Path) -> std::io::Result<PathBuf> {
    let mut dir = cwd.to_path_buf();
    loop {
        if workspace_file(&dir).is_some() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no WORKSPACE.toml found in current directory or any parent",
            ));
        }
    }
}

fn workspace_file(root: &Path) -> Option<PathBuf> {
    ["WORKSPACE.toml", "WORKSPACE"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|path| path.exists())
}

fn package_for_cwd(root: &Path, cwd: &Path) -> std::io::Result<String> {
    let mut package_dir = cwd.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not inside workspace {}",
                cwd.display(),
                root.display()
            ),
        )
    })?;
    loop {
        if root.join(package_dir).join("BUILD.toml").exists() || package_dir.as_os_str().is_empty()
        {
            return Ok(package_dir.to_string_lossy().trim_matches('/').to_string());
        }
        package_dir = package_dir.parent().unwrap_or_else(|| Path::new(""));
    }
}

fn root_relative(root: &Path, path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() || path.components().count() == 1 {
        path.to_string_lossy().to_string()
    } else {
        root.join(path).to_string_lossy().to_string()
    }
}

fn invalid_label(label: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("invalid label {label}; expected //package:target or :target"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_test_workspace_targets_build() {
        for workspace_root in test_workspace_roots() {
            let labels = workspace_labels(&workspace_root);
            assert!(
                !labels.is_empty(),
                "workspace {} should declare targets",
                workspace_root.display()
            );

            let workspace = Workspace::load_from(&workspace_root).unwrap();
            let mut executor = workspace.executor();
            let roots: Vec<_> = labels
                .iter()
                .map(|label| executor.add_task(label, None))
                .collect();
            let result = executor.run(&roots);
            let BuildResult::Success(output) = result else {
                panic!(
                    "workspace {} failed to build {:?}: {result:?}",
                    workspace_root.display(),
                    labels
                );
            };
            assert_eq!(
                output.outputs.len(),
                labels.len(),
                "workspace {} should emit one output per root target",
                workspace_root.display()
            );
        }
    }

    fn test_workspace_roots() -> Vec<PathBuf> {
        let test_workspaces = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_workspaces");
        let mut roots: Vec<_> = std::fs::read_dir(test_workspaces)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.join("WORKSPACE.toml").exists())
            .collect();
        roots.sort();
        roots
    }

    fn workspace_labels(root: &Path) -> Vec<String> {
        let mut labels = Vec::new();
        collect_workspace_labels(root, root, &mut labels);
        labels.sort();
        labels
    }

    fn collect_workspace_labels(root: &Path, dir: &Path, labels: &mut Vec<String>) {
        let build_file = dir.join("BUILD.toml");
        if build_file.exists() {
            let table = std::fs::read_to_string(&build_file)
                .unwrap()
                .parse::<toml::Table>()
                .unwrap();
            let package = dir
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .trim_matches('/')
                .to_string();
            for section in ["rust_binary", "rust_library"] {
                for target in table
                    .get(section)
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
                {
                    let name = target
                        .as_table()
                        .and_then(|target| target.get("name"))
                        .and_then(|name| name.as_str())
                        .expect("test workspace targets must have names");
                    labels.push(canonical_label(&Label {
                        package: package.clone(),
                        name: name.to_string(),
                    }));
                }
            }
        }

        let mut children: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        children.sort();
        for child in children {
            collect_workspace_labels(root, &child, labels);
        }
    }
}
