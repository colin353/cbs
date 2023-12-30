use sha2::Digest;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, RwLock};

#[derive(Debug, Clone)]
pub struct Context {
    pub start_time: std::time::Instant,
    pub actions: BuildActions,
    pub lockfile: Arc<HashMap<String, String>>,
    pub cache_dir: std::path::PathBuf,
    pub target: Option<String>,
    pub target_hash: Option<u64>,
    pub logs: Arc<RwLock<HashMap<String, Mutex<Vec<String>>>>>,
    pub config: Arc<HashMap<BuildConfigKey, String>>,
    pub hash: u64,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
pub enum BuildConfigKey {
    TargetFamily = 1,
    TargetEnv,
    TargetOS,
}

#[derive(Debug, Clone)]
pub struct BuildActions {}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: usize,
    pub dependencies: Vec<usize>,
    pub target: String,
    pub config: Option<Config>,
    pub result: Option<BuildResult>,
    pub available: bool,
    pub dependencies_ready: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuildResult {
    Success(BuildOutput),
    Failure(String),
}

pub mod BuildOutputKind {
    pub const TransitiveProducts: u32 = 0;
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct BuildOutput {
    pub outputs: Vec<std::path::PathBuf>,
    pub extras: HashMap<u32, Vec<String>>,
}

impl BuildOutput {
    pub fn get(&self, key: u32) -> &[String] {
        self.extras.get(&key).map(|s| s.as_slice()).unwrap_or(&[])
    }
}

impl BuildResult {
    pub fn noop() -> Self {
        BuildResult::Success(BuildOutput {
            outputs: Vec::new(),
            ..Default::default()
        })
    }

    pub fn merged<'a, I: Iterator<Item = &'a Self>>(results: I) -> Self {
        let mut outs = Vec::new();
        for result in results {
            match result {
                BuildResult::Success(BuildOutput { outputs, extras: _ }) => {
                    outs.extend(outputs.to_owned());
                }
                _ => return result.clone(),
            }
        }
        BuildResult::Success(BuildOutput {
            outputs: outs,
            ..Default::default()
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub dependencies: Vec<String>,
    pub build_plugin: String,
    pub location: Option<String>,
    pub sources: Vec<String>,
    pub build_dependencies: Vec<String>,
    pub kind: String,
    pub extras: HashMap<u32, Vec<String>>,
    pub hash: u64,
}

pub mod ConfigExtraKeys {
    pub const Features: u32 = 0;
}

impl Config {
    pub fn dependencies(&self) -> Vec<&str> {
        let mut out: Vec<_> = self.dependencies.iter().map(|s| s.as_str()).collect();
        out.push(self.build_plugin.as_str());
        out.extend(self.build_dependencies.iter().map(|s| s.as_str()));
        out
    }

    // Only possible when all dependencies are resolved
    pub fn calculate_hash(&mut self, context_hash: u64, deps_hash: u64) -> u64 {
        let mut hasher = sha2::Sha256::new();
        hasher.update(context_hash.to_be_bytes());
        hasher.update(deps_hash.to_be_bytes());
        for src in &self.sources {
            let mut buffer = [0; 1024];
            let f = match std::fs::File::open(src) {
                Ok(f) => f,
                Err(_) => {
                    // Sentinel value to represent missing/inaccessible file
                    hasher.update(&[0x12, 0x34]);
                    continue;
                }
            };
            let mut r = std::io::BufReader::new(f);
            loop {
                let count = match r.read(&mut buffer) {
                    Ok(c) => c,
                    Err(_) => {
                        hasher.update(&[0x56, 0x78]);
                        continue;
                    }
                };
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[0..count]);
            }
        }
        self.hash = u64::from_be_bytes(
            hasher.finalize()[..8]
                .try_into()
                .expect("invalid hash size"),
        );

        self.hash
    }

    pub fn get(&self, key: u32) -> &[String] {
        self.extras.get(&key).map(|s| s.as_slice()).unwrap_or(&[])
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskStatus {
    Resolving,
    Blocked,
    Building,
    Done,
}

impl Task {
    pub fn new(id: usize, target: String) -> Self {
        Self {
            id,
            dependencies: Vec::new(),
            target,
            config: None,
            result: None,
            available: true,
            dependencies_ready: 0,
        }
    }

    pub fn failure_stage(&self) -> TaskStatus {
        if self.config.is_none() {
            return TaskStatus::Resolving;
        }
        return TaskStatus::Building;
    }

    pub fn status(&self) -> TaskStatus {
        if self.result.is_some() {
            return TaskStatus::Done;
        }

        if self.config.is_none() {
            return TaskStatus::Resolving;
        }

        if self.dependencies_ready < self.dependencies.len() {
            return TaskStatus::Blocked;
        }

        if self.result.is_none() {
            return TaskStatus::Building;
        }

        TaskStatus::Done
    }
}

pub trait ResolverPlugin: std::fmt::Debug {
    fn can_resolve(&self, target: &str) -> bool;
    fn resolve(&self, context: Context, target: &str) -> std::io::Result<Config>;
}

pub trait BuildPlugin: std::fmt::Debug {
    fn build(
        &self,
        context: Context,
        task: Task,
        dependencies: HashMap<String, BuildOutput>,
    ) -> BuildResult;
}

#[derive(Debug)]
pub struct FakeBuilder {}

impl BuildPlugin for FakeBuilder {
    fn build(
        &self,
        context: Context,
        task: Task,
        dependencies: HashMap<String, BuildOutput>,
    ) -> BuildResult {
        BuildResult::noop()
    }
}

#[derive(Debug)]
pub struct FakeResolver {
    configs: HashMap<String, std::io::Result<Config>>,
}

impl FakeResolver {
    pub fn new() -> Self {
        Self {
            configs: HashMap::new(),
        }
    }

    pub fn with_configs(configs: Vec<(&str, std::io::Result<Config>)>) -> Self {
        Self {
            configs: configs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }
}

impl ResolverPlugin for FakeResolver {
    fn can_resolve(&self, target: &str) -> bool {
        true
    }

    fn resolve(&self, context: Context, target: &str) -> std::io::Result<Config> {
        match self.configs.get(target) {
            Some(Ok(c)) => Ok(c.clone()),
            Some(Err(e)) => Err(std::io::Error::new(e.kind(), "failed to read config")),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("failed to resolve target {target}"),
            )),
        }
    }
}

#[derive(Debug)]
pub struct FilesystemBuilder {}

impl BuildPlugin for FilesystemBuilder {
    fn build(
        &self,
        context: Context,
        task: Task,
        deps: HashMap<String, BuildOutput>,
    ) -> BuildResult {
        let loc = match task
            .config
            .expect("config must be resolved by now")
            .location
        {
            Some(l) => l,
            None => {
                return BuildResult::Failure(
                    "filesystem builder plugin requires a location set in the build config"
                        .to_string(),
                )
            }
        };
        BuildResult::Success(BuildOutput {
            outputs: vec![std::path::PathBuf::from(loc)],
            ..Default::default()
        })
    }
}

pub fn target_shortname(target: &str) -> &str {
    target
        .split("//")
        .last()
        .and_then(|s| s.split("/").last())
        .and_then(|s| s.split(":").last())
        .unwrap_or("")
}
