use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, bail};
use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand};

use gfiles::config::{self, Config};
use gfiles::search::{self, Query};
use gfiles::{classify, db, scan};

/// Catalog and organize your files across all your machines.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file [default: ~/.config/gfiles/config.toml]
    #[arg(long, global = true, env = "GFILES_CONFIG")]
    config: Option<PathBuf>,

    /// Catalog database [default: ~/.local/share/gfiles/catalog.db]
    #[arg(long, global = true, env = "GFILES_DB")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the volumes this machine scans
    #[command(subcommand)]
    Volume(VolumeCommand),

    /// Scan volumes into the catalog (all of them, or the ones named)
    Scan { volumes: Vec<String> },

    /// Find files by name or path words, kind, volume or machine
    Search {
        /// Words that must all appear in the name or path (prefix match, any case)
        terms: Vec<String>,
        #[arg(short, long, value_parser = PossibleValuesParser::new(classify::all_kinds()))]
        kind: Option<String>,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        machine: Option<String>,
        /// Maximum results
        #[arg(short = 'n', long, default_value_t = 50, conflicts_with = "all")]
        limit: usize,
        /// Return every result
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// Counts and sizes per kind for each volume
    Stats {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum VolumeCommand {
    /// Add a folder as a volume
    Add {
        path: PathBuf,
        /// Defaults to the folder's name
        #[arg(long)]
        name: Option<String>,
        /// Extra gitignore-style pattern to skip (repeatable)
        #[arg(long = "ignore")]
        ignore: Vec<String>,
    },
    /// List this machine's volumes
    List {
        #[arg(long)]
        json: bool,
    },
    /// Remove a volume from the config and its entries from the catalog
    Remove { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(gfiles::default_config_path);
    let db_path = cli.db.unwrap_or_else(gfiles::default_db_path);

    match cli.command {
        Command::Volume(VolumeCommand::Add { path, name, ignore }) => {
            let v = config::add_volume(&config_path, &path, name, ignore)?;
            println!(
                "Added volume {:?} ({}) to {}",
                v.name,
                v.path.display(),
                config_path.display()
            );
            println!("Run `gfiles scan {}` to catalog it.", v.name);
        }
        Command::Volume(VolumeCommand::List { json }) => {
            let config = Config::load(&config_path)?;
            if json {
                let list: Vec<_> = config
                    .volumes
                    .iter()
                    .map(|v| serde_json::json!({"id": v.id, "name": v.name, "path": v.path, "ignore": v.ignore}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if config.volumes.is_empty() {
                println!("No volumes yet. Add one with `gfiles volume add <path>`.");
            } else {
                for v in &config.volumes {
                    println!("{:<20} {}", v.name, v.path.display());
                }
            }
        }
        Command::Volume(VolumeCommand::Remove { name }) => {
            let id = config::remove_volume(&config_path, &name)?;
            let conn = db::open(&db_path)?;
            scan::forget_volume(&conn, &id)?;
            println!("Removed volume {name:?}");
        }
        Command::Scan { volumes } => {
            let config = Config::load(&config_path)?;
            if config.volumes.is_empty() {
                bail!("no volumes configured; add one with `gfiles volume add <path>`");
            }
            for name in &volumes {
                if config.volume(name).is_none() {
                    bail!("no volume named {name:?}");
                }
            }
            let machine = config.machine_name();
            let mut conn = db::open(&db_path)?;
            let mut failed = false;
            for v in config
                .volumes
                .iter()
                .filter(|v| volumes.is_empty() || volumes.contains(&v.name))
            {
                let started = Instant::now();
                match scan::scan_volume(&mut conn, v, &machine) {
                    Ok(s) => {
                        let mut line = format!(
                            "{}: {} files, {} repos, {} removed",
                            v.name, s.files, s.repos, s.removed
                        );
                        if s.non_utf8 > 0 {
                            line.push_str(&format!(", {} non-UTF-8 names skipped", s.non_utf8));
                        }
                        if s.errors > 0 {
                            line.push_str(&format!(", {} errors", s.errors));
                        }
                        println!("{line} ({:.1}s)", started.elapsed().as_secs_f64());
                    }
                    Err(e) => {
                        eprintln!("error: {e:#}");
                        failed = true;
                    }
                }
            }
            if failed {
                std::process::exit(1);
            }
        }
        Command::Search {
            terms,
            kind,
            volume,
            machine,
            limit,
            all,
            json,
        } => {
            let conn = db::open(&db_path)?;
            let limit = (!all).then_some(limit);
            let hits = search::search(
                &conn,
                &Query {
                    terms,
                    kind,
                    volume,
                    machine,
                    limit,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else {
                for h in &hits {
                    println!(
                        "{:<9}{:>9}  {}:{}",
                        h.kind,
                        human_size(h.size),
                        h.machine,
                        h.abs_path
                    );
                }
                if limit == Some(hits.len()) {
                    eprintln!(
                        "(showing the first {limit}; use -n or --all for more)",
                        limit = hits.len()
                    );
                }
            }
        }
        Command::Stats { json } => {
            let conn = db::open(&db_path)?;
            let stats = search::stats(&conn)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else if stats.is_empty() {
                println!("The catalog is empty. Run `gfiles scan`.");
            } else {
                for v in &stats {
                    println!("{}:{} ({})", v.machine, v.volume, v.root);
                    for k in &v.kinds {
                        println!("  {:<10}{:>9}{:>11}", k.kind, k.count, human_size(k.size));
                    }
                }
            }
        }
    }
    Ok(())
}

fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
