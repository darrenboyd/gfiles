pub mod classify;
pub mod config;
pub mod db;
pub mod fmt;
pub mod scan;
pub mod search;
pub mod tui;

use std::path::PathBuf;

/// Default config file: `~/.config/gfiles/config.toml` on Linux, `%APPDATA%` on Windows.
pub fn default_config_path() -> PathBuf {
    project_dirs().config_dir().join("config.toml")
}

/// Default catalog: `~/.local/share/gfiles/catalog.db` on Linux, `%APPDATA%` on Windows.
pub fn default_db_path() -> PathBuf {
    project_dirs().data_dir().join("catalog.db")
}

fn project_dirs() -> directories::ProjectDirs {
    directories::ProjectDirs::from("", "", "gfiles").expect("no home directory")
}
