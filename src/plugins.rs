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
            working_directory.join(format!("lib{crate_name}-{metadata}.{}", dylib_extension()))
        } else {
            working_directory.join(format!("lib{crate_name}-{metadata}.rlib"))
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
        let native_libs = match build_native_static_libs(context, &config, &working_directory) {
            Ok(libs) => libs,
            Err(e) => {
                return BuildResult::Failure(format!("failed to build native static libs: {e:?}"))
            }
        };

        let dependency_aliases = dependency_aliases(&config);
        let runtime_dependencies = runtime_dependencies(&config);
        let libraries: Vec<_> = runtime_dependencies
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

        let transitive_deps: Vec<(String, String)> = runtime_dependencies
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
        let native_link_args = native_libs
            .iter()
            .map(|lib| {
                vec![
                    "-L".to_string(),
                    format!(
                        "native={}",
                        lib.path
                            .parent()
                            .expect("native lib must have a parent")
                            .to_string_lossy()
                    ),
                    "-l".to_string(),
                    format!("static={}", lib.name),
                ]
                .into_iter()
            })
            .flatten();
        let transitive_native_link_args = native_link_args_from_products(&transitive_deps);

        let extern_crates = libraries
            .clone()
            .into_iter()
            .map(|(name, s)| vec!["--extern".to_string(), format!("{name}={}", s)].into_iter())
            .flatten();
        let proc_macro_extern = if crate_type == "proc-macro" {
            vec!["--extern".to_string(), "proc_macro".to_string()]
        } else {
            Vec::new()
        };

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
        args.extend(proc_macro_extern);
        args.extend(transitive_libraries);
        args.extend(native_link_args);
        args.extend(transitive_native_link_args);
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

        let rustc_env = rustc_env(&config);
        match context.actions.run_process_with_env(
            context,
            compiler,
            args.as_slice(),
            rustc_env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        ) {
            Ok(o) => o,
            Err(e) => return BuildResult::Failure(format!("failed to invoke compiler:\n{e}")),
        };

        let tdeps = transitive_deps
            .into_iter()
            .chain(native_libs.iter().map(|lib| {
                (
                    format!("native_{}", lib.name),
                    lib.path.display().to_string(),
                )
            }))
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
        let runtime_dependencies = runtime_dependencies(&config);
        let libraries: Vec<_> = runtime_dependencies
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

        let transitive_deps: Vec<(String, String)> = runtime_dependencies
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
        let transitive_native_link_args = native_link_args_from_products(&transitive_deps);

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
        args.extend(transitive_native_link_args);
        args.extend(features);
        args.extend(rustc_cfgs);
        args.extend(["-o".to_string(), out_file.to_string_lossy().to_string()]);
        args.push("--edition=2021".to_string());
        args.push("--color=always".to_string());

        let rustc_env = rustc_env(&config);
        match context.actions.run_process_with_env(
            context,
            compiler,
            args.as_slice(),
            rustc_env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        ) {
            Ok(o) => o,
            Err(e) => return BuildResult::Failure(format!("failed to invoke compiler:\n{e}")),
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

fn runtime_dependencies(config: &Config) -> Vec<String> {
    let mut deps = config.dependencies.clone();
    deps.extend(
        config
            .external_requirements
            .iter()
            .map(|requirement| requirement.target()),
    );
    deps.sort();
    deps.dedup();
    deps
}

fn rustc_env(config: &Config) -> Vec<(String, String)> {
    config
        .get(config_extra_keys::RUSTC_ENV)
        .iter()
        .filter_map(|encoded| {
            let (key, value) = encoded.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn native_link_args_from_products(products: &[(String, String)]) -> Vec<String> {
    products
        .iter()
        .filter_map(|(name, path)| {
            Some((
                name.strip_prefix("native_")?,
                std::path::Path::new(path).parent()?.to_string_lossy(),
            ))
        })
        .flat_map(|(name, parent)| {
            vec![
                "-L".to_string(),
                format!("native={parent}"),
                "-l".to_string(),
                format!("static={name}"),
            ]
        })
        .collect()
}

struct NativeStaticLib {
    name: String,
    sources: Vec<String>,
    include_dirs: Vec<String>,
    flags: Vec<String>,
}

struct NativeStaticLibOutput {
    name: String,
    path: std::path::PathBuf,
}

fn build_native_static_libs(
    context: &Context,
    config: &Config,
    working_directory: &std::path::Path,
) -> std::io::Result<Vec<NativeStaticLibOutput>> {
    let crate_root = match config.get(config_extra_keys::CRATE_ROOT).first() {
        Some(root) => std::path::PathBuf::from(root),
        None => return Ok(Vec::new()),
    };

    let native_dir = working_directory.join("native");
    std::fs::create_dir_all(&native_dir)?;

    config
        .get(config_extra_keys::NATIVE_STATIC_LIBS)
        .iter()
        .map(|encoded| {
            let lib = parse_native_static_lib(encoded)?;
            let lib_dir = native_dir.join(&lib.name);
            std::fs::create_dir_all(&lib_dir)?;

            let mut objects = Vec::new();
            for (idx, source) in lib.sources.iter().enumerate() {
                let source_path = crate_root.join(source);
                let object_path = lib_dir.join(format!("{idx}-{}.o", sanitize_path(source)));
                let mut args = vec![
                    "-c".to_string(),
                    source_path.to_string_lossy().to_string(),
                    "-o".to_string(),
                    object_path.to_string_lossy().to_string(),
                ];
                for include_dir in &lib.include_dirs {
                    args.push("-I".to_string());
                    args.push(crate_root.join(include_dir).to_string_lossy().to_string());
                }
                args.extend(lib.flags.iter().cloned());
                context.actions.run_process(context, "cc", &args)?;
                objects.push(object_path);
            }

            let archive_path = lib_dir.join(format!("lib{}.a", lib.name));
            let mut args = vec![
                "crs".to_string(),
                archive_path.to_string_lossy().to_string(),
            ];
            args.extend(
                objects
                    .iter()
                    .map(|object| object.to_string_lossy().to_string()),
            );
            context.actions.run_process(context, "ar", &args)?;

            Ok(NativeStaticLibOutput {
                name: lib.name,
                path: archive_path,
            })
        })
        .collect()
}

fn parse_native_static_lib(encoded: &str) -> std::io::Result<NativeStaticLib> {
    let mut parts = encoded.split('|');
    let name = parts.next().unwrap_or_default();
    if name.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "native static lib is missing a name",
        ));
    }

    Ok(NativeStaticLib {
        name: name.to_string(),
        sources: split_recipe_list(parts.next().unwrap_or_default()),
        include_dirs: split_recipe_list(parts.next().unwrap_or_default()),
        flags: split_recipe_list(parts.next().unwrap_or_default()),
    })
}

fn split_recipe_list(value: &str) -> Vec<String> {
    value
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn sanitize_path(path: &str) -> String {
    path.replace(['/', '.', '-'], "_")
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
