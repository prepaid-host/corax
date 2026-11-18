//! Main configuration file (`nagios.cfg`) and resource files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ConfigError;

/// A `broker_module=` line. corax does not load NEB modules, but it interprets the
/// arguments of the modules it replaces natively (Livestatus, npcdmod).
#[derive(Debug, Clone)]
pub struct BrokerModule {
    pub path: PathBuf,
    /// `key=value` arguments.
    pub options: HashMap<String, String>,
    /// Arguments without `=` (Livestatus: the socket path).
    pub positional: Vec<String>,
}

impl BrokerModule {
    fn parse(value: &str) -> Self {
        let mut parts = value.split_whitespace();
        let path = PathBuf::from(parts.next().unwrap_or_default());
        let mut options = HashMap::new();
        let mut positional = Vec::new();
        for part in parts {
            match part.split_once('=') {
                Some((k, v)) => {
                    options.insert(k.to_string(), v.to_string());
                }
                None => positional.push(part.to_string()),
            }
        }
        Self { path, options, positional }
    }

    fn file_name(&self) -> String {
        self.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn is_livestatus(&self) -> bool {
        self.file_name().starts_with("livestatus")
    }

    pub fn is_npcdmod(&self) -> bool {
        self.file_name().starts_with("npcdmod")
    }
}

/// Parsed main configuration file.
#[derive(Debug, Clone, Default)]
pub struct MainConfig {
    pub path: PathBuf,
    /// All `key=value` entries in file order (including repeated keys).
    pub entries: Vec<(String, String)>,
    pub cfg_files: Vec<PathBuf>,
    pub cfg_dirs: Vec<PathBuf>,
    pub resource_files: Vec<PathBuf>,
    pub broker_modules: Vec<BrokerModule>,
}

impl MainConfig {
    pub fn parse_file(path: &Path) -> Result<Self, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|source| ConfigError::Io { path: path.to_path_buf(), source })?;
        Self::parse_str(&text, path)
    }

    pub fn parse_str(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let base = path.parent().unwrap_or(Path::new("/"));
        let resolve = |v: &str| -> PathBuf {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                p
            } else {
                base.join(p)
            }
        };
        let mut cfg = MainConfig { path: path.to_path_buf(), ..Default::default() };
        for (idx, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::Syntax {
                    path: path.to_path_buf(),
                    line: idx + 1,
                    message: format!("expected 'key=value', got '{line}'"),
                });
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "cfg_file" => cfg.cfg_files.push(resolve(value)),
                "cfg_dir" => cfg.cfg_dirs.push(resolve(value)),
                "resource_file" => cfg.resource_files.push(resolve(value)),
                "broker_module" => cfg.broker_modules.push(BrokerModule::parse(value)),
                _ => {}
            }
            cfg.entries.push((key.to_string(), value.to_string()));
        }
        Ok(cfg)
    }

    /// Last value of `key`, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }

    pub fn path(&self, key: &str) -> Option<PathBuf> {
        self.get(key).filter(|v| !v.is_empty()).map(PathBuf::from)
    }

    pub fn bool_or(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            Some(v) => v.trim() != "0",
            None => default,
        }
    }

    pub fn f64_or(&self, key: &str, default: f64) -> f64 {
        self.get(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
    }

    pub fn i64_or(&self, key: &str, default: i64) -> i64 {
        self.get(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
    }

    pub fn livestatus_module(&self) -> Option<&BrokerModule> {
        self.broker_modules.iter().find(|m| m.is_livestatus())
    }

    pub fn npcd_module(&self) -> Option<&BrokerModule> {
        self.broker_modules.iter().find(|m| m.is_npcdmod())
    }
}

/// `$USERn$` macros from resource files.
#[derive(Debug, Clone, Default)]
pub struct Resources {
    pub user: HashMap<String, String>,
}

impl Resources {
    pub fn load(files: &[PathBuf]) -> Result<Self, ConfigError> {
        let mut res = Resources::default();
        for path in files {
            let text =
                std::fs::read_to_string(path).map_err(|source| ConfigError::Io { path: path.clone(), source })?;
            res.parse_into(&text);
        }
        Ok(res)
    }

    pub fn parse_into(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim().trim_matches('$');
                if key.starts_with("USER") {
                    self.user.insert(key.to_string(), value.trim().to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_livestatus_module() {
        let cfg = MainConfig::parse_str(
            "broker_module=/omd/sites/x/lib/mk-livestatus/livestatus.o num_client_threads=20 pnp_path=/p /omd/sites/x/tmp/run/live\ncfg_dir=conf.d\ninterval_length=60\n",
            Path::new("/etc/nagios/nagios.cfg"),
        )
        .unwrap();
        let ls = cfg.livestatus_module().unwrap();
        assert_eq!(ls.positional, vec!["/omd/sites/x/tmp/run/live"]);
        assert_eq!(ls.options["pnp_path"], "/p");
        assert_eq!(cfg.cfg_dirs, vec![PathBuf::from("/etc/nagios/conf.d")]);
        assert_eq!(cfg.i64_or("interval_length", 1), 60);
    }

    #[test]
    fn resources() {
        let mut r = Resources::default();
        r.parse_into("$USER1$=/omd/lib/plugins\n# x\n$USER4$=/omd/sites/x\n");
        assert_eq!(r.user["USER4"], "/omd/sites/x");
    }
}
