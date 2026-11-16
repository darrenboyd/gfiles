//! The per-machine config file: which machine this is and which volumes it scans.
//!
//! Volumes are added with `gfiles volume add`, which edits the file in place (keeping
//! comments) and gives each volume a UUID that stays fixed if it's renamed or moved.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Name this machine reports to the catalog. Defaults to the hostname.
    pub machine: Option<String>,
    #[serde(default, rename = "volume")]
    pub volumes: Vec<VolumeConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeConfig {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    /// Extra gitignore-style patterns, on top of the built-in defaults.
    #[serde(default)]
    pub ignore: Vec<String>,
}

const NEW_FILE_HEADER: &str = "\
# gfiles config for this machine.
# machine = \"name\"   # defaults to the hostname
# Add volumes with `gfiles volume add <path>`.
";

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn machine_name(&self) -> String {
        self.machine
            .clone()
            .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned())
    }

    pub fn volume(&self, name: &str) -> Option<&VolumeConfig> {
        self.volumes.iter().find(|v| v.name == name)
    }
}

/// Adds a volume to the config file, creating the file if needed. Returns the new volume.
pub fn add_volume(
    config_path: &Path,
    path: &Path,
    name: Option<String>,
    ignore: Vec<String>,
) -> Result<VolumeConfig> {
    let config = Config::load(config_path)?;
    let path = path
        .canonicalize()
        .with_context(|| format!("volume path {}", path.display()))?;
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let name = match name {
        Some(n) => n,
        None => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .context("can't derive a name from this path; pass --name")?,
    };
    if config.volume(&name).is_some() {
        bail!("a volume named {name:?} already exists");
    }
    for v in &config.volumes {
        if path.starts_with(&v.path) || v.path.starts_with(&path) {
            bail!(
                "{} overlaps volume {:?} ({}); volumes must not nest",
                path.display(),
                v.name,
                v.path.display()
            );
        }
    }
    let path_str = path
        .to_str()
        .context("volume path is not valid UTF-8")?
        .to_string();

    let is_new = !config_path.exists();
    let mut doc = read_doc(config_path)?;
    let volume = VolumeConfig {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        path,
        ignore,
    };
    let mut table = Table::new();
    table.insert("id", value(&volume.id));
    table.insert("name", value(&volume.name));
    table.insert("path", value(path_str));
    if !volume.ignore.is_empty() {
        let patterns: toml_edit::Array = volume.ignore.iter().map(String::as_str).collect();
        table.insert("ignore", value(patterns));
    }
    match doc.get_mut("volume") {
        Some(Item::ArrayOfTables(arr)) => arr.push(table),
        Some(_) => bail!(
            "`volume` in {} is not an array of tables",
            config_path.display()
        ),
        None => {
            let mut arr = ArrayOfTables::new();
            arr.push(table);
            doc.insert("volume", Item::ArrayOfTables(arr));
        }
    }
    let text = if is_new {
        format!("{NEW_FILE_HEADER}\n{doc}")
    } else {
        doc.to_string()
    };
    write_text(config_path, &text)?;
    Ok(volume)
}

/// Removes a volume from the config file. Returns its id.
pub fn remove_volume(config_path: &Path, name: &str) -> Result<String> {
    let config = Config::load(config_path)?;
    let Some(volume) = config.volume(name) else {
        bail!("no volume named {name:?}");
    };
    let mut doc = read_doc(config_path)?;
    if let Some(Item::ArrayOfTables(arr)) = doc.get_mut("volume") {
        arr.retain(|t| t.get("name").and_then(|n| n.as_str()) != Some(name));
    }
    write_text(config_path, &doc.to_string())?;
    Ok(volume.id.clone())
}

fn read_doc(path: &Path) -> Result<DocumentMut> {
    let text = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    text.parse()
        .with_context(|| format!("parsing config {}", path.display()))
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, text).with_context(|| format!("writing config {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_remove_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config/config.toml");
        let books = tmp.path().join("Books");
        let music = tmp.path().join("Music");
        fs::create_dir_all(&books).unwrap();
        fs::create_dir_all(books.join("inner")).unwrap();
        fs::create_dir_all(&music).unwrap();

        let v = add_volume(&cfg, &books, None, vec!["*.part".into()]).unwrap();
        assert_eq!(v.name, "Books");
        add_volume(&cfg, &music, Some("tunes".into()), vec![]).unwrap();

        let loaded = Config::load(&cfg).unwrap();
        assert_eq!(loaded.volumes.len(), 2);
        assert_eq!(loaded.volume("Books").unwrap().ignore, vec!["*.part"]);
        assert_eq!(loaded.volume("Books").unwrap().id, v.id);
        assert!(
            fs::read_to_string(&cfg)
                .unwrap()
                .starts_with("# gfiles config")
        );

        assert!(add_volume(&cfg, &books, Some("again".into()), vec![]).is_err());
        assert!(add_volume(&cfg, &books.join("inner"), None, vec![]).is_err());
        assert!(add_volume(&cfg, tmp.path(), Some("parent".into()), vec![]).is_err());

        assert_eq!(remove_volume(&cfg, "Books").unwrap(), v.id);
        let loaded = Config::load(&cfg).unwrap();
        assert_eq!(loaded.volumes.len(), 1);
        assert_eq!(loaded.volumes[0].name, "tunes");
    }
}
