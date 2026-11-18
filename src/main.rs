use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, bail};
use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand};

use gfiles::apply::{self, Op};
use gfiles::config::{self, Config};
use gfiles::dupes::{self, Filters};
use gfiles::plan::{self, Spec};
use gfiles::search::{self, Query};
use gfiles::{classify, db, fmt, mirror, overlap, scan, store};

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

    /// Compute full content hashes for a volume's files, or a folder's, to confirm duplicates
    Hash {
        volume: String,
        /// A folder inside the volume, like Photos/2024
        folder: Option<String>,
    },

    /// Fetch each remote's catalog over SSH into this one (see [[remote]] in the config)
    Pull,

    /// Scan every volume, then pull from each remote. Run this from a timer.
    Sync,

    /// Write this machine's scanned volumes to stdout (run by `pull` over SSH; read-only)
    Export,

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

    /// Files with the same content in more than one place, most wasted space first
    Dupes {
        #[command(flatten)]
        filters: FilterArgs,
        /// Maximum groups
        #[arg(short = 'n', long, default_value_t = 20, conflicts_with = "all")]
        limit: usize,
        /// Every group
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// Files with no copy on another machine, by folder, most bytes first
    Coverage {
        #[command(flatten)]
        filters: FilterArgs,
        /// List the files of one folder from the report (like elrond:Media/Photos/2019)
        #[arg(long)]
        folder: Option<String>,
        /// Maximum folders
        #[arg(short = 'n', long, default_value_t = 30, conflicts_with = "all")]
        limit: usize,
        /// Every folder
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// How well each [[mirror]] is in sync, and folder pairs that look like mirrors
    Mirrors {
        /// Maximum files listed per kind of difference
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },

    /// Folder pairs on one machine holding the same files, most shared space first
    Overlaps {
        #[arg(long)]
        machine: Option<String>,
        /// Maximum pairs
        #[arg(short = 'n', long, default_value_t = 30, conflicts_with = "all")]
        limit: usize,
        /// Every pair
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// What emptying a folder of its duplicates would free, and what would stay (saves nothing)
    Drain {
        /// Like elrond:darren/todo
        folder: String,
        /// A folder inside it to leave out entirely (repeatable)
        #[arg(long)]
        except: Vec<String>,
        /// Maximum folders listed as holding the copies that stay
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },

    /// Cleanup plans: build, review, apply, undo
    #[command(subcommand)]
    Plan(PlanCommand),

    /// Serve the catalog to AI tools over MCP (stdin/stdout)
    Mcp,

    /// Counts and sizes per kind for each volume
    Stats {
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct FilterArgs {
    #[arg(short, long, value_parser = PossibleValuesParser::new(classify::all_kinds()))]
    kind: Option<String>,
    #[arg(long)]
    volume: Option<String>,
    #[arg(long)]
    machine: Option<String>,
    /// Leave out files smaller than this (like 500K, 100M, 2G)
    #[arg(long, value_parser = fmt::parse_size, default_value = "0")]
    min_size: i64,
}

impl FilterArgs {
    fn into_filters(self, limit: usize, all: bool) -> Filters {
        Filters {
            kind: self.kind,
            volume: self.volume,
            machine: self.machine,
            min_size: self.min_size,
            limit: (!all).then_some(limit),
        }
    }
}

#[derive(Subcommand)]
enum PlanCommand {
    /// Plan removing every file in these folders that has a copy elsewhere on the machine
    Drain {
        /// Like elrond:darren/todo (all on one machine)
        #[arg(required = true)]
        folders: Vec<String>,
        /// A folder inside them to leave out entirely (repeatable)
        #[arg(long)]
        except: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Plan removing from one folder every file another folder also holds
    Keep {
        /// The folder whose copies stay
        keeper: String,
        /// The folder files are removed from
        #[arg(long)]
        over: String,
        /// A folder inside --over to leave out entirely (repeatable)
        #[arg(long)]
        except: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Every plan, newest first
    List {
        #[arg(long)]
        json: bool,
    },
    /// A plan's totals, the files it removes, and the files it leaves
    Show {
        id: String,
        /// Maximum files and folders listed
        #[arg(short = 'n', long, default_value_t = 30, conflicts_with = "all")]
        limit: usize,
        #[arg(long)]
        all: bool,
        /// A folder in the plan (machine:volume/dir): list only what it removes there, break
        /// down what's left in it one level down, and list its biggest leftover files
        #[arg(long)]
        under: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Move a plan's files to each volume's trash (asks you to type the plan id)
    Apply { id: String },
    /// Put a plan's files back (asks you to type the plan id)
    Undo { id: String },
    /// Delete a plan's files from the trash for good (asks you to type the plan id)
    Empty { id: String },
    /// Delete a draft plan
    Drop { id: String },
    /// Run a plan step for the hub, on stdin/stdout (run by `plan apply` over SSH)
    #[command(hide = true)]
    Exec,
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
        let conn = open_for_queries(&config_path, &db_path)?;
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
            if let Some(fs) = gfiles::platform::network_fs(&v.path) {
                eprintln!(
                    "warning: {} is on a network filesystem ({fs}): scanning reads every new \
                     file over the network, and the files belong to another machine. Scan them \
                     there instead and pull its catalog (see [[remote]] in docs/setup.md).",
                    v.path.display()
                );
            }
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
        Command::Hash { volume, folder } => {
            let config = Config::load(&config_path)?;
            let Some(vol) = config.volume(&volume) else {
                let names: Vec<&str> = config.volumes.iter().map(|v| v.name.as_str()).collect();
                bail!(
                    "no volume named {volume:?}; this machine has: {}",
                    names.join(", ")
                );
            };
            let mut conn = db::open(&db_path)?;
            let started = Instant::now();
            let s = scan::hash_volume(&mut conn, vol, folder.as_deref())?;
            println!(
                "{}: {} hashed, {} already hashed, {} skipped ({:.1}s)",
                vol.name,
                s.hashed,
                s.already,
                s.skipped,
                started.elapsed().as_secs_f64()
            );
            if s.skipped > 0 {
                println!(
                    "Skipped files were unreadable or changed since the last scan; run `gfiles scan` first."
                );
            }
        }
        Command::Pull => {
            let config = Config::load(&config_path)?;
            let mut conn = db::open(&db_path)?;
            if config.remotes.is_empty() {
                println!("No [[remote]] in the config, so there's nothing to pull.");
            }
            if !pull(&config, &mut conn)? {
                std::process::exit(1);
            }
        }
        Command::Sync => {
            let config = Config::load(&config_path)?;
            let mut conn = db::open(&db_path)?;
            let scanned = scan_volumes(&config, &mut conn, &[])?;
            // Pull even if a local volume failed to scan: it keeps its previous entries, and
            // the remotes are still worth fetching.
            let pulled = pull(&config, &mut conn)?;
            if !(scanned && pulled) {
                std::process::exit(1);
            }
        }
        Command::Export => {
            let config = Config::load(&config_path)?;
            let conn = db::open(&db_path)?;
            let ids: Vec<String> = config.volumes.iter().map(|v| v.id.clone()).collect();
            gfiles::sync::export(
                &conn,
                &config.machine_name(),
                &ids,
                std::io::stdout().lock(),
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
            let conn = open_for_queries(&config_path, &db_path)?;
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
                    let copies = h.copies.map_or(String::new(), |n| format!("×{n}"));
                    println!(
                        "{:<9}{:>9} {:>3}  {}:{}",
                        h.kind,
                        fmt::size(h.size),
                        copies,
                        h.machine,
                        h.abs_path
                    );
                    for r in &h.replicas {
                        println!("{:23}{}:{}", "", r.machine, r.abs_path);
                    }
                }
                if total > hits.len() as i64 {
                    eprintln!(
                        "(showing {} of {total}; use -n or --all for more)",
                        hits.len()
                    );
                }
            }
        }
        Command::Dupes {
            filters,
            limit,
            all,
            json,
        } => {
            let conn = open_for_queries(&config_path, &db_path)?;
            let filters = filters.into_filters(limit, all);
            filters.check(&conn)?;
            let d = dupes::duplicates(&conn, &filters)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                print_duplicates(&d);
            }
        }
        Command::Coverage {
            filters,
            folder,
            limit,
            all,
            json,
        } => {
            let conn = open_for_queries(&config_path, &db_path)?;
            let filters = filters.into_filters(limit, all);
            filters.check(&conn)?;
            let c = dupes::coverage(&conn, &filters, folder.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&c)?);
            } else {
                print_coverage(&c);
            }
        }
        Command::Mirrors { limit, json } => {
            let conn = open_for_queries(&config_path, &db_path)?;
            let health = mirror::health(&conn, limit)?;
            let suggestions = mirror::suggestions(&conn)?;
            if json {
                let out = serde_json::json!({
                    "mirrors": health,
                    "suggestions": suggestions,
                    "warnings": mirror::warnings(&conn)?,
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                print_mirrors(&health, &suggestions);
            }
        }
        Command::Overlaps {
            machine,
            limit,
            all,
            json,
        } => {
            let conn = open_for_queries(&config_path, &db_path)?;
            let pairs = overlap::pairs(&conn, machine.as_deref(), (!all).then_some(limit))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pairs)?);
            } else {
                print_overlaps(&pairs);
            }
        }
        Command::Drain {
            folder,
            except,
            limit,
            json,
        } => {
            let conn = open_for_queries(&config_path, &db_path)?;
            let folder = mirror::parse_folder(&folder)?;
            let except = parse_folders(&except)?;
            let p = overlap::drain_preview(&conn, &folder, &except, Some(limit))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&p)?);
            } else {
                print_drain(&p);
            }
        }
        Command::Plan(cmd) => plan_command(cmd, &config_path, &db_path)?,
        Command::Mcp => {
            let conn = open_for_queries(&config_path, &db_path)?;
            let config = Config::load(&config_path)?;
            gfiles::mcp::serve(
                &conn,
                Some(&config),
                std::io::stdin().lock(),
                std::io::stdout().lock(),
            )?;
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

/// Opens the catalog with the config's mirrors loaded, for commands that answer questions.
/// Mirror warnings (a mirror skipped because its volume isn't in the catalog) go to stderr.
fn open_for_queries(
    config_path: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<rusqlite::Connection> {
    let config = Config::load(config_path)?;
    let conn = db::open(db_path)?;
    let warnings = mirror::load(&conn, &config)?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    Ok(conn)
}

fn plan_command(
    cmd: PlanCommand,
    config_path: &std::path::Path,
    db_path: &std::path::Path,
) -> Result<()> {
    let lock_path = db_path.with_file_name("actions.lock");
    match cmd {
        PlanCommand::Drain {
            folders,
            except,
            json,
        } => {
            let spec = Spec::Drain(parse_folders(&folders)?);
            make_plan(config_path, db_path, spec, &parse_folders(&except)?, json)
        }
        PlanCommand::Keep {
            keeper,
            over,
            except,
            json,
        } => {
            let spec = Spec::Keep {
                keeper: mirror::parse_folder(&keeper)?,
                over: mirror::parse_folder(&over)?,
            };
            make_plan(config_path, db_path, spec, &parse_folders(&except)?, json)
        }
        PlanCommand::List { json } => {
            let conn = db::open(db_path)?;
            let plans = plan::list(&conn)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plans)?);
            } else if plans.is_empty() {
                println!("No plans yet. Find folders to clean up with `gfiles overlaps`.");
            } else {
                for p in &plans {
                    println!(
                        "{}  {:<8} {:<5} {:>7} files {:>10}  {}",
                        p.id,
                        p.state,
                        p.kind,
                        p.remove.files,
                        fmt::size(p.remove.bytes),
                        plan_scope(p)
                    );
                }
            }
            Ok(())
        }
        PlanCommand::Show {
            id,
            limit,
            all,
            under,
            json,
        } => {
            let conn = db::open(db_path)?;
            let under = under.as_deref().map(mirror::parse_folder).transpose()?;
            let view = plan::show(&conn, &id, (!all).then_some(limit), under.as_ref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
            } else {
                print_plan(&view);
            }
            Ok(())
        }
        PlanCommand::Drop { id } => {
            let conn = db::open(db_path)?;
            plan::drop(&conn, &id)?;
            println!("Dropped plan {id}.");
            Ok(())
        }
        PlanCommand::Apply { id } => run_plan(config_path, db_path, &lock_path, Op::Apply, &id),
        PlanCommand::Undo { id } => run_plan(config_path, db_path, &lock_path, Op::Undo, &id),
        PlanCommand::Empty { id } => run_plan(config_path, db_path, &lock_path, Op::Empty, &id),
        PlanCommand::Exec => {
            // Only the hub runs this, over SSH, after Darren confirmed the plan there. Run
            // locally it would skip that confirmation, so it refuses.
            if std::env::var_os("SSH_CONNECTION").is_none() {
                bail!(
                    "plan exec only runs over SSH from the hub; use `gfiles plan apply`, \
                     `undo` or `empty`, which ask you to confirm"
                );
            }
            let config = Config::load(config_path)?;
            apply::serve(
                &config,
                &lock_path,
                std::io::stdin().lock(),
                std::io::stdout().lock(),
            )
        }
    }
}

fn parse_folders(folders: &[String]) -> Result<Vec<mirror::FolderRef>> {
    folders.iter().map(|f| mirror::parse_folder(f)).collect()
}

fn make_plan(
    config_path: &std::path::Path,
    db_path: &std::path::Path,
    spec: Spec,
    except: &[mirror::FolderRef],
    json: bool,
) -> Result<()> {
    let config = Config::load(config_path)?;
    let conn = open_for_queries(config_path, db_path)?;
    let built = plan::build(&conn, spec, except)?;
    plan::check_not_synced(&conn, &built, &config.not_synced_folders()?)?;
    let now = scan_now();
    let id = plan::save(&conn, &built, now)?;
    let view = plan::show(&conn, &id, Some(20), None)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        print_plan(&view);
        println!(
            "\nSaved as a draft. Review it with `gfiles plan show {id} --all`; apply it with \
             `gfiles plan apply {id}`."
        );
    }
    Ok(())
}

fn scan_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Applies, undoes or empties a plan after Darren types its id on the terminal.
fn run_plan(
    config_path: &std::path::Path,
    db_path: &std::path::Path,
    lock_path: &std::path::Path,
    op: Op,
    id: &str,
) -> Result<()> {
    let config = Config::load(config_path)?;
    let mut conn = db::open(db_path)?;
    let s = plan::summary(&conn, id)?;
    let what = match op {
        Op::Apply => format!(
            "move {} files ({}) on {} to the trash",
            s.remove.files - s.moved.files - s.skipped.files,
            fmt::size(s.remove.bytes - s.moved.bytes - s.skipped.bytes),
            s.machine
        ),
        Op::Undo => format!(
            "put back the {} files ({}) plan {id} moved on {}",
            s.moved.files,
            fmt::size(s.moved.bytes),
            s.machine
        ),
        Op::Empty => format!(
            "permanently delete the {} files ({}) plan {id} moved to the trash on {}",
            s.moved.files,
            fmt::size(s.moved.bytes),
            s.machine
        ),
    };
    let route = apply::Route::for_machine(&config, &s.machine, lock_path)?;
    // Name the folders, so a plan drafted the wrong way round (by an agent, say) shows here.
    println!("Plan {id}: {}", plan_folders(&s));
    println!("This will {what}.");
    let typed = gfiles::platform::ask_terminal(&format!(
        "Type the plan id to {}: ",
        apply::op_name(op)
    ))
    .map_err(|e| {
        anyhow::anyhow!(
            "plan {} needs you at a terminal to confirm, and there isn't one ({e}); nothing was done",
            apply::op_name(op)
        )
    })?;
    if typed != id {
        println!("That isn't the plan id; nothing was done.");
        return Ok(());
    }
    let started = Instant::now();
    let done = apply::execute(&mut conn, &route, op, id)?;
    let after = plan::summary(&conn, id)?;
    match op {
        Op::Apply => {
            println!(
                "Moved {} files, skipped {} ({:.1}s). Plan {id} is {}.",
                done.moved,
                done.skipped,
                started.elapsed().as_secs_f64(),
                after.state
            );
            if done.skipped > 0 {
                println!("`gfiles plan show {id}` lists the skipped files and why.");
            }
            println!(
                "The files are in .gfiles-trash/{id} on each volume. Check things over, then \
                 `gfiles plan empty {id}` frees the space (or `gfiles plan undo {id}`)."
            );
        }
        Op::Undo => println!(
            "Put back {} files. Plan {id} is {}.",
            done.restored, after.state
        ),
        Op::Empty => println!(
            "Deleted {} files ({}). Plan {id} is {}.",
            done.deleted,
            fmt::size(done.deleted_bytes as i64),
            after.state
        ),
    }
    if !done.left_in_trash.is_empty() {
        println!("Left in the trash ({}):", done.left_in_trash.len());
        for l in done.left_in_trash.iter().take(20) {
            println!("  {l}");
        }
    }
    Ok(())
}

fn tally(t: &plan::Tally) -> String {
    format!("{} files, {}", t.files, fmt::size(t.bytes))
}

fn print_overlaps(p: &overlap::Pairs) {
    let pairs = &p.pairs;
    if pairs.is_empty() {
        println!("No folders share files on one machine.");
    }
    for p in pairs {
        if p.a == p.b {
            println!(
                "{:>10} shared  within {} ({})",
                fmt::size(p.shared.bytes),
                p.a,
                tally(&p.a_total)
            );
        } else {
            let side = |f: &mirror::FolderRef, total: &plan::Tally, only: &plan::Tally| {
                format!(
                    "{f} ({}; {} only here)",
                    tally(total),
                    fmt::size(only.bytes)
                )
            };
            println!(
                "{:>10} shared  {}\n{:18}{}",
                fmt::size(p.shared.bytes),
                side(&p.a, &p.a_total, &p.a_only),
                "",
                side(&p.b, &p.b_total, &p.b_only)
            );
        }
    }
    if p.total > pairs.len() {
        eprintln!(
            "(showing the top {} of {}; use -n or --all for more)",
            pairs.len(),
            p.total
        );
    }
    if !pairs.is_empty() {
        println!("\nSee what draining one would free with `gfiles drain <folder>`.");
    }
}

fn print_drain(p: &overlap::Preview) {
    if p.except.is_empty() {
        println!("{}: {}", p.folder, tally(&p.total));
    } else {
        let except: Vec<String> = p.except.iter().map(|e| e.to_string()).collect();
        println!(
            "{} except {}: {}",
            p.folder,
            except.join(", "),
            tally(&p.total)
        );
    }
    println!(
        "A drain would remove {}, whose copies stay in:",
        tally(&p.removable)
    );
    for c in &p.copies_in {
        println!(
            "  {:>7} files {:>10}  {}",
            c.tally.files,
            fmt::size(c.tally.bytes),
            c.folder
        );
    }
    if p.copies_in_total > p.copies_in.len() {
        println!(
            "  and {} more folders",
            p.copies_in_total - p.copies_in.len()
        );
    }
    if !p.left.is_empty() {
        println!("It would leave:");
        for l in &p.left {
            println!(
                "  {:>7} files {:>10}  {}",
                l.tally.files,
                fmt::size(l.tally.bytes),
                l.reason
            );
        }
    }
    let except: String = p.except.iter().map(|e| format!(" --except {e}")).collect();
    println!("\nPlan it with `gfiles plan drain {}{except}`.", p.folder);
}

/// A plan's kind and folders, as `drain a b` or `keep a over b`.
fn plan_folders(s: &plan::Summary) -> String {
    format!("{} {}", s.kind, plan_scope(s))
}

/// A plan's folders, as `a b`, `a over b`, plus `except c` for the folders it leaves out.
fn plan_scope(s: &plan::Summary) -> String {
    let mut text = s
        .folders
        .join(if s.kind == "keep" { " over " } else { " " });
    if !s.except.is_empty() {
        text.push_str(&format!(" except {}", s.except.join(", ")));
    }
    text
}

fn print_plan(v: &plan::View) {
    let s = &v.summary;
    println!(
        "Plan {} ({}, {}): {}",
        s.id,
        s.state,
        fmt::date_time(s.created_at),
        plan_folders(s)
    );
    println!("  removes {}", tally(&s.remove));
    println!("  leaves  {}", tally(&s.left));
    if s.moved.files + s.skipped.files > 0 {
        println!("  moved   {}", tally(&s.moved));
        println!("  skipped {}", tally(&s.skipped));
    }
    let scope = match &v.under {
        Some(u) => format!(" in {u}"),
        None => String::new(),
    };
    if !v.items.is_empty() {
        println!(
            "\nRemoves{scope}: {} files (the copy that stays below each):",
            v.items_total
        );
        for i in &v.items {
            let status = match (&i.outcome, &i.reason) {
                (Some(o), Some(r)) => format!("  [{o}: {r}]"),
                (Some(o), None) => format!("  [{o}]"),
                _ => String::new(),
            };
            println!("{:>10}  {}{status}", fmt::size(i.size), i.abs_path);
            println!("{:>10}  = {}", "", i.keep_abs_path);
        }
        if v.items_total > v.items.len() as i64 {
            println!(
                "  ... and {} more (use -n or --all)",
                v.items_total - v.items.len() as i64
            );
        }
    } else if v.under.is_some() {
        println!("\nRemoves nothing{scope}.");
    }
    if !v.left_by_reason.is_empty() {
        println!("\nLeaves{scope}:");
        for g in &v.left_by_reason {
            println!(
                "  {:>7} files {:>10}  {}",
                g.files,
                fmt::size(g.bytes),
                g.reason
            );
        }
        println!("\nLeaves{scope}, by folder:");
        for f in &v.left_by_folder {
            let name = if f.here {
                format!("{} (files directly in it)", f.folder)
            } else {
                f.folder.to_string()
            };
            let reasons = match f.reasons.as_slice() {
                [only] => only.reason.clone(),
                many => many
                    .iter()
                    .map(|g| format!("{} {}", g.reason, fmt::size(g.bytes)))
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            println!(
                "  {:>7} files {:>10}  {name}  ({reasons})",
                f.files,
                fmt::size(f.bytes)
            );
        }
        if v.left_folders_total > v.left_by_folder.len() {
            println!(
                "  ... and {} more folders (use -n or --all)",
                v.left_folders_total - v.left_by_folder.len()
            );
        }
        if v.under.is_some() {
            println!("\nBiggest files left:");
            for l in &v.left_files {
                println!("{:>10}  {}  ({})", fmt::size(l.size), l.abs_path, l.reason);
            }
        } else {
            println!(
                "\nBreak a folder down with `gfiles plan show {} --under <folder>`.",
                s.id
            );
        }
    } else if let Some(u) = &v.under {
        println!("\nThe plan leaves nothing in {u}.");
    }
}

/// One copy on a line: its location, or a mirror file and its replicas.
fn copy_line(c: &dupes::Copy) -> String {
    let loc = |l: &dupes::Location| format!("{}:{}", l.machine, l.abs_path);
    match (&c.mirror, &c.mirror_path) {
        (Some(m), Some(p)) => format!(
            "mirror {m}: {p} ({})",
            c.locations.iter().map(loc).collect::<Vec<_>>().join(", ")
        ),
        _ => loc(&c.locations[0]),
    }
}

fn print_duplicates(d: &dupes::Duplicates) {
    if d.groups.is_empty() {
        println!("No duplicates found.");
    }
    for g in &d.groups {
        println!(
            "{} copies of {} {}, {} wasted{}",
            g.copies.len(),
            fmt::size(g.size),
            g.kind,
            fmt::size(g.wasted),
            if g.confirmed { "" } else { " (probable)" }
        );
        for c in &g.copies {
            println!("  {}", copy_line(c));
        }
    }
    if d.total_groups > d.groups.len() as i64 {
        eprintln!(
            "(showing {} of {} groups; use -n or --all for more)",
            d.groups.len(),
            d.total_groups
        );
    }
}

fn print_coverage(c: &dupes::Coverage) {
    let tally = |t: &dupes::Tally| format!("{} files, {}", t.files, fmt::size(t.bytes));
    println!("Uncovered (one place only): {}", tally(&c.uncovered));
    println!("Probably covered:           {}", tally(&c.probably_covered));
    println!("Covered (confirmed):        {}", tally(&c.covered));
    if c.not_yet_fingerprinted.files > 0 {
        println!(
            "Not yet fingerprinted:      {}",
            tally(&c.not_yet_fingerprinted)
        );
    }
    if let Some(files) = &c.files {
        println!();
        for f in files {
            println!(
                "{:<17}{:>9}  {}",
                f.status,
                fmt::size(f.size),
                copy_line(&f.copy)
            );
        }
        return;
    }
    if !c.folders.is_empty() {
        println!("\n  uncovered          probably covered   folder");
    }
    for f in &c.folders {
        println!(
            "{:>6} {:>10}  {:>6} {:>10}   {}",
            f.uncovered.files,
            fmt::size(f.uncovered.bytes),
            f.probably_covered.files,
            fmt::size(f.probably_covered.bytes),
            f.folder
        );
    }
    if c.total_folders > c.folders.len() as i64 {
        eprintln!(
            "(showing {} of {} folders; use -n or --all for more, --folder to list one)",
            c.folders.len(),
            c.total_folders
        );
    }
}

fn print_mirrors(health: &[mirror::Health], suggestions: &[mirror::Suggestion]) {
    if health.is_empty() {
        println!("No mirrors configured.");
    }
    for h in health {
        println!(
            "Mirror {}: {}",
            h.mirror,
            if h.in_sync { "in sync" } else { "out of sync" }
        );
        for r in &h.replicas {
            println!(
                "  {:<40} {:>7} files, newest {}, scanned {}",
                r.folder,
                r.files,
                r.newest_mtime.map_or("-".into(), fmt::date),
                r.last_scan_at.map_or("never".into(), fmt::date_time),
            );
        }
        let list = |title: &str, l: &mirror::Listed| {
            if l.total > 0 {
                println!("  {title} ({}):", l.total);
                for item in &l.items {
                    println!("    {item}");
                }
                if l.total > l.items.len() {
                    println!("    ... and {} more (use -n)", l.total - l.items.len());
                }
            }
        };
        for o in &h.only_in {
            list(&format!("Only in {}", o.replica), &o.files);
        }
        list("Same path, different content", &h.differing);
        list("Conflict copies", &h.conflicts);
        list(
            "Changed since another replica was scanned (not drift; rescan it to compare)",
            &h.not_yet_seen,
        );
        list(
            "Same file, named differently by the sync tools",
            &h.named_differently,
        );
        if h.skipped_placeholders > 0 {
            println!(
                "  ({} Google-native stand-ins and ignored files left out)",
                h.skipped_placeholders
            );
        }
    }
    if !suggestions.is_empty() {
        println!(
            "\nThese folders look like mirrors. If a sync tool keeps them the same, add to the config:"
        );
        for s in suggestions {
            println!(
                "\n# {} of {} files match ({:.0}%)\n{}",
                s.matching,
                s.files[0].max(s.files[1]),
                s.ratio * 100.0,
                s.config
            );
        }
        println!("(Don't add a folder that's a backup copy: a mirror counts as one place.)");
    }
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
                if s.fingerprinted > 0 {
                    line.push_str(&format!(", {} fingerprinted", s.fingerprinted));
                }
                if s.unfingerprinted > 0 {
                    line.push_str(&format!(
                        ", {} couldn't be fingerprinted (unreadable or changing)",
                        s.unfingerprinted
                    ));
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

/// Pulls every remote, then forgets the volumes of machines that are neither this one nor a
/// configured remote (a remote taken out of the config). Returns false if any remote failed
/// (after reporting it); the others are still applied.
fn pull(config: &Config, conn: &mut rusqlite::Connection) -> Result<bool> {
    let machine = config.machine_name();
    let mut ok = true;
    for remote in &config.remotes {
        let started = Instant::now();
        match gfiles::sync::pull(conn, &machine, remote) {
            Ok(reports) => {
                println!(
                    "Pulled {} ({:.1}s):",
                    remote.name,
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
                                "  {name}: no longer configured on {}, so removed",
                                remote.name
                            )
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                ok = false;
            }
        }
    }
    let mut known: Vec<String> = config.remotes.iter().map(|r| r.name.clone()).collect();
    known.push(machine);
    for (machine, name) in store::forget_machines_except(conn, &known)? {
        println!("{machine}:{name}: {machine} is no longer a remote, so removed");
    }
    Ok(ok)
}
