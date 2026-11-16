use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, bail};
use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand};

use gfiles::config::{self, Config};
use gfiles::search::{self, Query};
use gfiles::{classify, db, fmt, scan, store};

/// Catalog and organize your files across all your machines.
///
/// Run with no command for the interactive TUI. Enter prints the selected path, so
/// `xdg-open "$(gfiles)"` opens the file you pick.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file [default: ~/.config/gfiles/config.toml]
    #[arg(long, global = true, env = "GFILES_CONFIG")]
    config: Option<PathBuf>,

    /// Catalog database [default: ~/.local/share/gfiles/catalog.db]
    #[arg(long, global = true, env = "GFILES_DB")]
    db: Option<PathBuf>,

    /// Leave out to open the interactive TUI
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the volumes this machine scans
    #[command(subcommand)]
    Volume(VolumeCommand),

    /// Scan volumes into the catalog (all of them, or the ones named)
    Scan { volumes: Vec<String> },

    /// Send this machine's scanned volumes to the central catalog (see [central] in the config)
    Push,

    /// Scan every volume, then push if a central catalog is configured. Run this from a timer.
    Sync,

    /// Receive volumes pushed from another machine on stdin (run by `push` over SSH)
    Ingest {
        /// The machine this push may write for; refuses a push claiming any other. Set it in
        /// the forced command of that machine's key in authorized_keys.
        #[arg(long)]
        machine: String,
    },

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

    /// Serve the catalog to AI tools over MCP (stdin/stdout)
    Mcp,

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

    let Some(command) = cli.command else {
        let conn = db::open(&db_path)?;
        if let Some(path) = gfiles::tui::run(conn)? {
            println!("{path}");
        }
        return Ok(());
    };

    match command {
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
            store::forget_volume(&conn, &id)?;
            println!("Removed volume {name:?}");
        }
        Command::Scan { volumes } => {
            let config = Config::load(&config_path)?;
            let mut conn = db::open(&db_path)?;
            if !scan_volumes(&config, &mut conn, &volumes)? {
                std::process::exit(1);
            }
        }
        Command::Push => {
            let config = Config::load(&config_path)?;
            let conn = db::open(&db_path)?;
            push(&config, &conn)?;
        }
        Command::Sync => {
            let config = Config::load(&config_path)?;
            let mut conn = db::open(&db_path)?;
            let scanned = scan_volumes(&config, &mut conn, &[])?;
            // Push even if some volume failed to scan: the others are still worth sending, and
            // a failed volume keeps its previous entries.
            if config.central.is_some() {
                push(&config, &conn)?;
            }
            if !scanned {
                std::process::exit(1);
            }
        }
        Command::Ingest { machine } => {
            let config = Config::load(&config_path)?;
            let mut conn = db::open(&db_path)?;
            let mut stdout = std::io::stdout().lock();
            gfiles::sync::ingest(
                &mut conn,
                &config.machine_name(),
                &machine,
                std::io::stdin().lock(),
                |report| {
                    serde_json::to_writer(&mut stdout, report)?;
                    writeln!(stdout)?;
                    Ok(stdout.flush()?)
                },
            )?;
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
            let query = Query {
                terms,
                kind,
                volume,
                machine,
                limit,
            };
            search::check_filters(&conn, &query)?;
            let hits = search::search(&conn, &query)?;
            let total = if limit == Some(hits.len()) {
                search::count(&conn, &query)?
            } else {
                hits.len() as i64
            };
            if json {
                // The same shape as the MCP search_files tool, so a cut-off list is visible.
                let out = serde_json::json!({"total_matches": total, "files": hits});
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for h in &hits {
                    println!(
                        "{:<9}{:>9}  {}:{}",
                        h.kind,
                        fmt::size(h.size),
                        h.machine,
                        h.abs_path
                    );
                }
                if total > hits.len() as i64 {
                    eprintln!(
                        "(showing {} of {total}; use -n or --all for more)",
                        hits.len()
                    );
                }
            }
        }
        Command::Mcp => {
            let conn = db::open(&db_path)?;
            gfiles::mcp::serve(&conn, std::io::stdin().lock(), std::io::stdout().lock())?;
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
                        println!("  {:<10}{:>9}{:>11}", k.kind, k.count, fmt::size(k.size));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Scans the named volumes, or all of them. Returns false if any failed (after reporting it).
fn scan_volumes(
    config: &Config,
    conn: &mut rusqlite::Connection,
    names: &[String],
) -> Result<bool> {
    if config.volumes.is_empty() {
        bail!("no volumes configured; add one with `gfiles volume add <path>`");
    }
    for name in names {
        if config.volume(name).is_none() {
            bail!("no volume named {name:?}");
        }
    }
    let machine = config.machine_name();
    let mut ok = true;
    for v in config
        .volumes
        .iter()
        .filter(|v| names.is_empty() || names.contains(&v.name))
    {
        let started = Instant::now();
        match scan::scan_volume(conn, v, &machine) {
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
                if s.kept > 0 {
                    line.push_str(&format!(
                        ", {} unseen entries kept because they couldn't be read",
                        s.kept
                    ));
                }
                println!("{line} ({:.1}s)", started.elapsed().as_secs_f64());
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                ok = false;
            }
        }
    }
    Ok(ok)
}

fn push(config: &Config, conn: &rusqlite::Connection) -> Result<()> {
    let Some(central) = &config.central else {
        bail!("no [central] section in the config, so there's nowhere to push");
    };
    let ids: Vec<String> = config.volumes.iter().map(|v| v.id.clone()).collect();
    let started = Instant::now();
    let reports = gfiles::sync::push(conn, central, &config.machine_name(), &ids)?;
    println!(
        "Pushed to {} ({:.1}s):",
        central.host,
        started.elapsed().as_secs_f64()
    );
    for r in &reports {
        match r {
            gfiles::sync::Report::Volume {
                name,
                entries,
                removed,
            } => println!("  {name}: {entries} entries, {removed} removed"),
            gfiles::sync::Report::Forgot { name } => {
                println!(
                    "  {name}: no longer configured here, so removed from {}",
                    central.host
                )
            }
        }
    }
    Ok(())
}
