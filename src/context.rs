use crate::core::{BuildActions, BuildConfigKey, Context, Task};
use sha2::Digest;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

impl Context {
    pub fn new<T: IntoIterator<Item = (BuildConfigKey, String)>>(
        cache_dir: std::path::PathBuf,
        config: T,
    ) -> Self {
        Self {
            actions: BuildActions::new(),
            lockfile: Arc::new(HashMap::new()),
            start_time: std::time::Instant::now(),
            cache_dir,
            target: None,
            target_hash: None,
            logs: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(config.into_iter().collect()),
            hash: 0,
        }
    }

    pub fn get_config(&self, key: BuildConfigKey) -> Option<&str> {
        self.config.get(&key).map(|s| s.as_str())
    }

    pub fn calculate_hash(&mut self) -> u64 {
        let mut hasher = sha2::Sha256::new();
        let mut cfg_values: Vec<_> = self.config.iter().collect();
        cfg_values.sort_by_key(|(k, _)| **k as u32);
        for (k, v) in cfg_values {
            hasher.update((*k as u32).to_be_bytes());
            hasher.update(v.as_bytes());
        }
        self.hash = u64::from_be_bytes(
            hasher.finalize()[..8]
                .try_into()
                .expect("invalid hash size"),
        );
        self.hash
    }

    pub fn with_target(&self, target: &str) -> Self {
        let mut s = self.clone();
        s.target = Some(target.to_string());
        s
    }

    pub fn with_task(&self, task: &Task) -> Self {
        let mut s = self.clone();
        s.target = Some(task.target.clone());
        s.target_hash = task.config.as_ref().map(|c| c.hash);
        s
    }

    pub fn get_locked_version(&self, target: &str) -> std::io::Result<String> {
        self.lockfile
            .get(target)
            .map(|s| s.to_string())
            .ok_or(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{target} does not have a lockfile entry!"),
            ))
    }

    pub fn log<S: Into<String>>(&self, message: S) {
        let target = match self.target.as_ref() {
            Some(t) => t,
            None => {
                println!("{}", message.into());
                return;
            }
        };

        {
            let _logs = self.logs.read().expect("failed to acquire log lock");
            if let Some(logs) = _logs.get(target) {
                logs.lock()
                    .expect("failed to acquire target log lock")
                    .push(message.into());

                return;
            }
        }

        self.logs
            .write()
            .expect("failed to acquire log writelock")
            .insert(target.to_string(), Mutex::new(vec![message.into()]));
    }

    pub fn scratch_dir(&self) -> std::path::PathBuf {
        match (self.target.as_ref(), self.target_hash.as_ref()) {
            (Some(t), None) => {
                let v = self
                    .get_locked_version(&t)
                    .unwrap_or_else(|_| String::new());
                self.cache_dir
                    .join("resolve")
                    .join("scratch")
                    .join(format!("{}-{v}", to_dir(t)))
            }
            (Some(t), Some(h)) => self
                .cache_dir
                .join("build")
                .join("scratch")
                .join(format!("{}-{h:x}", to_dir(t))),
            (None, None) => self.cache_dir.clone(),
            _ => panic!("must have attached target if hash is present!"),
        }
    }

    pub fn working_directory(&self) -> std::path::PathBuf {
        match (self.target.as_ref(), self.target_hash.as_ref()) {
            (Some(t), None) => {
                let v = self
                    .get_locked_version(&t)
                    .unwrap_or_else(|_| String::new());
                self.cache_dir
                    .join("resolve")
                    .join(format!("{}-{v}", to_dir(t)))
            }
            (Some(t), Some(h)) => self
                .cache_dir
                .join("build")
                .join(format!("{}-{h:x}", to_dir(t))),
            (None, None) => self.cache_dir.clone(),
            _ => panic!("must have attached target if hash is present!"),
        }
    }
}

fn to_dir(name: &str) -> String {
    name.replace(&[':', '/', '@'], "_")
}
