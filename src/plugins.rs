use std::collections::HashMap;

use crate::core::*;

pub mod plugin_kind {
    pub const RUST_LIBRARY: &str = "rust_library";
    pub const RUST_BINARY: &str = "rust_binary";
}

#[derive(Debug)]
pub struct RustPlugin {}

impl RustPlugin {
    fn build_library(
        &self,
        context: &Context,
        name: &str,
        config: Config,
        deps: HashMap<String, BuildOutput>,
    ) -> BuildResult {
        let compiler = match config
            .build_dependencies
            .get(0)
            .and_then(|t| deps.get(t))
            .and_then(|f| f.outputs.get(0))
        {
            Some(t) => t,
            None => {
                return BuildResult::Failure(
                    "the rust compiler must be specified as a build_dependency!".to_string(),
                )
            }
        };

        let working_directory = context.working_directory();
        std::fs::create_dir_all(&working_directory).ok();
        let crate_name = config
            .get(config_extra_keys::CRATE_NAME)
            .first()
            .map(|s| s.as_str())
            .unwrap_or(name);
        let crate_type = config
            .get(config_extra_keys::CRATE_TYPE)
            .first()
            .map(|s| s.as_str())
            .unwrap_or("rlib");
        let edition = config
            .get(config_extra_keys::EDITION)
            .first()
            .map(|s| s.as_str())
            .unwrap_or("2018");
        let metadata = context
            .target_hash
            .map(|hash| format!("{hash:x}"))
            .unwrap_or_else(|| config.hash.to_string());
        let out_file = if crate_type == "proc-macro" {
            working_directory.join(format!("lib{name}-{metadata}.{}", dylib_extension()))
        } else {
            working_directory.join(format!("lib{name}-{metadata}.rlib"))
        };

        let root_source: String = match config.get(config_extra_keys::ROOT_SOURCE).first() {
            Some(s) => s.clone(),
            None => {
                let mut root_source_candidates: Vec<_> = config
                    .sources
                    .iter()
                    .filter(|s| s.ends_with("lib.rs") || s.ends_with(&format!("{name}.rs")))
                    .collect();
                root_source_candidates.sort_by_key(|c| c.split('/').count());
                match root_source_candidates.into_iter().next() {
                    Some(s) => s.clone(),
                    None => {
                        return BuildResult::Failure(format!(
                            "no main.rs or {name}.rs source file specified!"
                        ))
                    }
                }
            }
        };

        let dependency_aliases = dependency_aliases(&config);
        let libraries: Vec<_> = config
            .dependencies
            .iter()
            .map(|t| {
                deps.get(t)
                    .expect("dependencies must be built by now!")
                    .outputs
                    .iter()
                    .map(|d| {
                        (
                            dependency_aliases
                                .get(t)
                                .cloned()
                                .unwrap_or_else(|| rust_name(t)),
                            d.as_path().display().to_string(),
                        )
                    })
            })
            .flatten()
            .collect();

        let transitive_deps: Vec<(String, String)> = config
            .dependencies
            .iter()
            .map(|t| {
                deps.get(t)
                    .expect("dependencies must be built by now!")
                    .get(build_output_kind::TRANSITIVE_PRODUCTS)
                    .iter()
                    .filter_map(move |d| {
                        let mut components = d.splitn(2, ':');
                        let name = components.next()?;
                        let path = components.next()?;
                        Some((name.to_string(), path.to_string()))
                    })
            })
            .flatten()
            .chain(libraries.clone())
            .collect();

        let transitive_libraries = transitive_deps
            .iter()
            .map(|(_, path)| {
                vec![
                    "-L".to_string(),
                    std::path::Path::new(&path)
                        .parent()
                        .expect("must have a parent...")
                        .to_string_lossy()
                        .to_string(),
                ]
                .into_iter()
            })
            .flatten();

        let extern_crates = libraries
            .clone()
            .into_iter()
            .map(|(name, s)| vec!["--extern".to_string(), format!("{name}={}", s)].into_iter())
            .flatten();

        let features = config
            .get(config_extra_keys::FEATURES)
            .iter()
            .map(|s| vec!["--cfg".to_string(), format!("feature=\"{s}\"")].into_iter())
            .flatten();
        let rustc_cfgs = config
            .get(config_extra_keys::RUSTC_CFGS)
            .iter()
            .map(|s| vec!["--cfg".to_string(), s.to_string()].into_iter())
            .flatten();

        let mut args: Vec<String> = Vec::new();
        args.push(root_source);
        args.extend(extern_crates);
        args.extend(transitive_libraries);
        args.extend(features);
        args.extend(rustc_cfgs);

        args.push(format!("--edition={edition}"));

        args.extend([
            "-C".to_string(),
            format!("metadata={metadata}"),
            "--crate-type".to_string(),
            crate_type.to_string(),
            "--crate-name".to_string(),
            crate_name.to_string(),
            "-o".to_string(),
            out_file.to_string_lossy().to_string(),
            "--cap-lints".to_string(),
            "allow".to_string(),
            "--color=always".to_string(),
        ]);

        match context
            .actions
            .run_process(context, compiler, args.as_slice())
        {
            Ok(o) => o,
            Err(e) => return BuildResult::Failure(format!("failed to invoke compiler: {e:?}")),
        };

        let tdeps = transitive_deps
            .into_iter()
            .map(|(name, path)| format!("{name}:{path}"))
            .collect();

        let mut extras = HashMap::new();
        extras.insert(build_output_kind::TRANSITIVE_PRODUCTS, tdeps);

        BuildResult::Success(BuildOutput {
            outputs: vec![std::path::PathBuf::from(
                out_file.to_string_lossy().to_string(),
            )],
            extras,
        })
    }

    fn build_binary(
        &self,
        context: &Context,
        name: &str,
        config: Config,
        deps: HashMap<String, BuildOutput>,
    ) -> BuildResult {
        let compiler = match config
            .build_dependencies
            .get(0)
            .and_then(|t| deps.get(t))
            .and_then(|f| f.outputs.get(0))
        {
            Some(t) => t,
            None => {
                return BuildResult::Failure(
                    "the rust compiler must be specified as a build_dependency!".to_string(),
                )
            }
        };

        let working_directory = context.working_directory();
        std::fs::create_dir_all(&working_directory).ok();
        let out_file = working_directory.join(name);

        let dependency_aliases = dependency_aliases(&config);
        let libraries: Vec<_> = config
            .dependencies
            .iter()
            .map(|t| {
                deps.get(t)
                    .expect("dependencies must be built by now!")
                    .outputs
                    .iter()
                    .map(|d| {
                        (
                            dependency_aliases
                                .get(t)
                                .cloned()
                                .unwrap_or_else(|| rust_name(t)),
                            d.as_path().display().to_string(),
                        )
                    })
            })
            .flatten()
            .collect();

        let transitive_deps: Vec<(String, String)> = config
            .dependencies
            .iter()
            .map(|t| {
                deps.get(t)
                    .expect("dependencies must be built by now!")
                    .get(build_output_kind::TRANSITIVE_PRODUCTS)
                    .iter()
                    .filter_map(move |d| {
                        let mut components = d.splitn(2, ':');
                        let name = components.next()?;
                        let path = components.next()?;
                        Some((name.to_string(), path.to_string()))
                    })
            })
            .flatten()
            .chain(libraries.clone())
            .collect();

        let transitive_libraries = transitive_deps
            .iter()
            .map(|(_, path)| {
                vec![
                    "-L".to_string(),
                    std::path::Path::new(&path)
                        .parent()
                        .expect("must have a parent...")
                        .to_string_lossy()
                        .to_string(),
                ]
                .into_iter()
            })
            .flatten();

        let extern_crates = libraries
            .clone()
            .into_iter()
            .map(|(name, s)| vec!["--extern".to_string(), format!("{name}={}", s)].into_iter())
            .flatten();

        let features = config
            .get(config_extra_keys::FEATURES)
            .iter()
            .map(|s| vec!["--cfg".to_string(), format!("feature=\"{s}\"")].into_iter())
            .flatten();
        let rustc_cfgs = config
            .get(config_extra_keys::RUSTC_CFGS)
            .iter()
            .map(|s| vec!["--cfg".to_string(), s.to_string()].into_iter())
            .flatten();

        let mut root_source_candidates: Vec<_> = config
            .sources
            .iter()
            .filter(|s| s.ends_with("/main.rs") || s.ends_with(&format!("/{name}.rs")))
            .collect();
        root_source_candidates.sort_by_key(|c| c.split('/').count());
        let root_source = match root_source_candidates.into_iter().next() {
            Some(s) => s.to_string(),
            None => {
                return BuildResult::Failure(format!(
                    "no main.rs or {name}.rs source file specified!"
                ))
            }
        };

        let mut args: Vec<String> = Vec::new();
        args.push(root_source);
        args.extend(extern_crates);
        args.extend(transitive_libraries);
        args.extend(features);
        args.extend(rustc_cfgs);
        args.extend(["-o".to_string(), out_file.to_string_lossy().to_string()]);
        args.push("--edition=2021".to_string());
        args.push("--color=always".to_string());

        match context
            .actions
            .run_process(context, compiler, args.as_slice())
        {
            Ok(o) => o,
            Err(e) => return BuildResult::Failure(format!("failed to invoke compiler: {e:?}")),
        };

        BuildResult::Success(BuildOutput {
            outputs: vec![std::path::PathBuf::from(out_file)],
            ..Default::default()
        })
    }
}

fn rust_name(target: &str) -> String {
    crate::core::target_shortname(target)
        .split('@')
        .next()
        .unwrap_or("")
        .replace('-', "_")
}

fn dependency_aliases(config: &Config) -> HashMap<String, String> {
    config
        .get(config_extra_keys::DEPENDENCY_ALIASES)
        .iter()
        .filter_map(|alias| {
            let (target, crate_name) = alias.rsplit_once('=')?;
            Some((target.to_string(), crate_name.to_string()))
        })
        .collect()
}

fn dylib_extension() -> &'static str {
    match std::env::consts::OS {
        "macos" => "dylib",
        "windows" => "dll",
        _ => "so",
    }
}

impl BuildPlugin for RustPlugin {
    fn build(
        &self,
        context: Context,
        task: Task,
        deps: HashMap<String, BuildOutput>,
    ) -> BuildResult {
        let name = rust_name(&task.target);

        let config = task.config.expect("config must be specified by now");
        if config.kind == plugin_kind::RUST_LIBRARY {
            self.build_library(&context, &name, config, deps)
        } else if config.kind == plugin_kind::RUST_BINARY {
            self.build_binary(&context, &name, config, deps)
        } else {
            BuildResult::Failure(format!("unsupported target kind: {:?}", config.kind))
        }
    }
}
