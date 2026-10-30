//! The plugin registry, the file that lists the plugins the shell loads at startup. It is
//! `$MEERSH_PLUGINS`, or `plugins.json` in the meersh data directory.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use meersh_engine::Engine;
use serde::{Deserialize, Serialize};

/// The variable that moves the registry file, for tests and for separate setups.
pub const ENV_VAR: &str = "MEERSH_PLUGINS";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub plugins: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// The commands as of `plugin add`. The shell asks the plugin again at every start.
    #[serde(default)]
    pub commands: Vec<String>,
}

/// The registry file. The shell's environment counts, so `env set MEERSH_PLUGINS ...` and
/// tests can point it elsewhere without changing the process environment.
pub fn registry_path(engine: &Engine) -> Option<PathBuf> {
    engine
        .env_vars()
        .remove(ENV_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(dirs::data_dir()?.join("meersh").join("plugins.json")))
}

/// The name of a plugin is its file name without a `meersh_plugin_` or `meersh-plugin-`
/// prefix, so `meersh_plugin_example` is called `example`.
pub fn plugin_name(path: &Path) -> String {
    let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let short = file.strip_prefix("meersh_plugin_").or_else(|| file.strip_prefix("meersh-plugin-"));
    short.filter(|s| !s.is_empty()).unwrap_or(&file).to_string()
}

impl Registry {
    /// A missing file is an empty registry.
    pub fn load(path: &Path) -> Result<Registry, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} is not a valid plugin registry ({e})", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(format!("cannot read {} ({e})", path.display())),
        }
    }

    /// Writes a temporary file next to the registry and renames it, so a crash never leaves
    /// half a registry behind.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let fail = |e: std::io::Error| format!("cannot write {} ({e})", path.display());
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(fail)?;
        }
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        let text = serde_json::to_string_pretty(self).expect("the registry serializes");
        std::fs::write(&tmp, text + "\n").map_err(fail)?;
        std::fs::rename(&tmp, path).map_err(fail)
    }

    /// The entry with this name, or with this path.
    pub fn position(&self, name: &str, path: Option<&Path>) -> Option<usize> {
        self.plugins.iter().position(|e| e.name == name || path.is_some_and(|p| e.path == p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_drop_the_plugin_prefix() {
        assert_eq!(plugin_name(Path::new("/x/meersh_plugin_example")), "example");
        assert_eq!(plugin_name(Path::new("/x/meersh-plugin-git")), "git");
        assert_eq!(plugin_name(Path::new("/x/tool")), "tool");
        assert_eq!(plugin_name(Path::new("/x/meersh_plugin_")), "meersh_plugin_");
    }

    #[test]
    fn the_registry_round_trips_through_its_file() {
        let dir = std::env::temp_dir().join(format!("meersh-plugin-registry-{}", std::process::id()));
        let file = dir.join("nested").join("plugins.json");
        assert_eq!(Registry::load(&file), Ok(Registry::default()));
        let registry = Registry {
            plugins: vec![Entry {
                name: "example".into(),
                path: "/bin/example".into(),
                commands: vec!["words".into()],
            }],
        };
        registry.save(&file).unwrap();
        assert_eq!(Registry::load(&file), Ok(registry.clone()));
        assert_eq!(registry.position("example", None), Some(0));
        assert_eq!(registry.position("other", Some(Path::new("/bin/example"))), Some(0));
        assert_eq!(registry.position("other", None), None);
        std::fs::write(&file, "not json").unwrap();
        assert!(Registry::load(&file).unwrap_err().contains("not a valid plugin registry"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
