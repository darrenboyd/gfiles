//! `gfiles mcp`: a Model Context Protocol server on stdin/stdout, so AI tools can query the
//! catalog. Works over SSH too: `ssh <host> gfiles mcp`.
//!
//! The protocol layer is written by hand: newline-delimited JSON-RPC 2.0 with just the methods
//! a tools-only server needs (`initialize`, `ping`, `tools/list`, `tools/call`). That keeps the
//! crate synchronous, with no async runtime. Stdout carries protocol messages only; anything
//! else goes to stderr. Every tool is read-only, except that `draft_cleanup_plan` and
//! `draft_plan_from_list` save a draft plan in the catalog. No tool touches files: only Darren applies a plan, at his terminal.

use std::io::{BufRead, Write};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::config::Config;
use crate::dupes::{self, Filters};
use crate::plan::{self, Spec};
use crate::search::{self, Query};
use crate::{classify, mirror, overlap};

/// Protocol versions this server can speak; it answers with the client's if listed, else the
/// newest. Tools-only servers behave the same under all of them.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const SEARCH_DEFAULT_LIMIT: usize = 50;
const SEARCH_MAX_LIMIT: usize = 500;
const FOLDERS_DEFAULT_LIMIT: usize = 30;
const GROUPS_DEFAULT_LIMIT: usize = 20;
const MIRROR_LIST_LIMIT: usize = 50;
/// Entries `draft_plan_from_list` takes inline; longer lists come as a file.
const LIST_INLINE_MAX: usize = 1000;
/// The biggest list file `draft_plan_from_list` reads.
const LIST_FILE_MAX: u64 = 64 * 1024 * 1024;

const INSTRUCTIONS: &str = "\
gfiles is a catalog of the user's own files (not system files) across all their machines. \
Each file belongs to a volume (a scanned folder) on a machine, and has a kind: book, photo, \
music, video, document, archive, code, repo (a whole git repository) or other. \
Kinds come from file extensions, so PDFs are 'document' even when they are books. \
Start with catalog_overview to see what exists. For 'where are my X?' questions use \
file_locations, which groups matches by folder; use search_files to list specific files. \
Paths are absolute on the machine named in each result, which may not be this one. \
A mirror is a folder kept the same on several machines by a sync tool (like Google Drive): \
its files appear once, with their other replicas, and a mirror counts as one place for \
backups, since the sync tool copies deletes too. Files match by a sampled fingerprint of \
their content: 'probable' or 'probably covered' means the full content wasn't compared yet \
(the user can run `gfiles hash` to confirm); files whose full hashes differ never match. \
A cleanup plan's 'confirmed' total counts removals whose full hashes already agree; the rest \
are probable, and apply compares full contents before moving each. Use find_duplicates for \
wasted space, backup_coverage for files with no copy on another machine, and mirror_health to \
see whether mirrors are in sync. To reclaim duplicate space on one machine: folder_overlaps finds folder \
pairs holding the same files, drain_preview shows what emptying a folder would free and what \
would stay, and draft_cleanup_plan saves a reviewable plan. When you have worked out what \
should change yourself (files to file into another folder, or copies that hold the same thing \
in different bytes, like a photo whose metadata a photo manager rewrote), draft_plan_from_list \
saves your list of moves and removals as a plan, with any entry it rejects and why. Mark a \
removal vouched, with a reason, only when you know the two files hold the same thing although \
their bytes differ: apply then doesn't compare their content. Tools never move or delete files: \
the user applies a plan themselves with `gfiles plan apply <id>` at a terminal. Applying moves \
files to a trash folder on their volume, where `gfiles plan undo` can put them back until \
`gfiles plan empty` deletes them for good. Plans go draft, applying, applied, then undone or \
emptied. Never run plan apply, undo, empty or exec for them, by any means.";

struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError {
            code,
            message: message.into(),
        }
    }
}

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Serves requests from `input` until it closes. With `config`, mirrors are reloaded before
/// every tool call, so a long-running server sees volumes pulled since it started.
pub fn serve(
    conn: &Connection,
    config: Option<&Config>,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(conn, config, &line) {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// One message in, at most one response out. Notifications and responses get no reply.
fn handle_line(conn: &Connection, config: Option<&Config>, line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                RpcError::new(PARSE_ERROR, e.to_string()),
            ));
        }
    };
    let id = msg.get("id").cloned();
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // A response to us (we send no requests) or junk. Reply only to junk that has an id.
        return match id {
            Some(id) if msg.get("result").is_none() && msg.get("error").is_none() => Some(
                error_response(id, RpcError::new(INVALID_REQUEST, "missing method")),
            ),
            _ => None,
        };
    };
    let id = id?; // notifications (no id), e.g. notifications/initialized
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    Some(match handle_request(conn, config, method, &params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => error_response(id, e),
    })
}

fn error_response(id: Value, e: RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.code, "message": e.message}})
}

const TOOLS: &[&str] = &[
    "catalog_overview",
    "file_locations",
    "search_files",
    "find_duplicates",
    "backup_coverage",
    "mirror_health",
    "folder_overlaps",
    "drain_preview",
    "draft_cleanup_plan",
    "draft_plan_from_list",
    "list_cleanup_plans",
    "show_cleanup_plan",
];

fn handle_request(
    conn: &Connection,
    config: Option<&Config>,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = requested
                .filter(|v| PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "gfiles", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::new(INVALID_PARAMS, "missing tool name"))?;
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            // Failures inside a tool are reported in the result, so the model can see and
            // correct them; only an unknown tool is a protocol error.
            if !TOOLS.contains(&name) {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    format!("unknown tool {name:?}"),
                ));
            }
            let reload = || match config {
                Some(c) => mirror::load(conn, c).map(drop),
                None => Ok(()),
            };
            let result = reload().and_then(|()| match name {
                "catalog_overview" => catalog_overview(conn),
                "file_locations" => file_locations(conn, &args),
                "search_files" => search_files(conn, &args),
                "find_duplicates" => find_duplicates(conn, &args),
                "backup_coverage" => backup_coverage(conn, &args),
                "folder_overlaps" => folder_overlaps(conn, &args),
                "drain_preview" => drain_preview(conn, &args),
                "draft_cleanup_plan" => draft_cleanup_plan(conn, config, &args),
                "draft_plan_from_list" => draft_plan_from_list(conn, config, &args),
                "list_cleanup_plans" => Ok(json!({"plans": plan::list(conn)?})),
                "show_cleanup_plan" => show_cleanup_plan(conn, &args),
                _ => mirror_health(conn, &args),
            });
            Ok(match result {
                Ok(value) => json!({
                    "content": [{"type": "text", "text": value.to_string()}],
                    "structuredContent": value,
                    "isError": false,
                }),
                Err(e) => json!({
                    "content": [{"type": "text", "text": format!("{e:#}")}],
                    "isError": true,
                }),
            })
        }
        _ => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!("unknown method {method:?}"),
        )),
    }
}

fn tool_definitions() -> Value {
    let kinds: Vec<&str> = classify::all_kinds().collect();
    let filters = json!({
        "query": {
            "type": "string",
            "description": "Words that must all appear in the file name or folder path. Each \
                word matches as a prefix, ignoring case and accents (\"tolk hob\" finds \
                \"Tolkien/The Hobbit.epub\"). Leave out to match everything."
        },
        "kind": {"type": "string", "enum": kinds, "description": "Only this kind of file."},
        "volume": {"type": "string", "description": "Only this volume (see catalog_overview)."},
        "machine": {"type": "string", "description": "Only this machine (see catalog_overview)."},
    });
    let with_limit = |default: usize, max: usize| {
        let mut props = filters.clone();
        props["limit"] = json!({
            "type": "integer", "minimum": 1, "maximum": max,
            "description": format!("Most results to return (default {default}, max {max}).")
        });
        props
    };
    let mut content_filters = with_limit(GROUPS_DEFAULT_LIMIT, SEARCH_MAX_LIMIT);
    content_filters.as_object_mut().unwrap().remove("query");
    content_filters["min_size"] = json!({
        "type": "integer", "minimum": 0,
        "description": "Leave out files smaller than this many bytes."
    });
    let mut coverage_props = content_filters.clone();
    coverage_props["folder"] = json!({
        "type": "string",
        "description": "A folder value from an earlier backup_coverage result: lists that \
            folder's files that need a backup."
    });
    json!([
        {
            "name": "catalog_overview",
            "title": "Catalog overview",
            "description": "Every machine and volume in the catalog, when each was last \
                scanned, and how many files of each kind (with total size) it holds. \
                Call this first to learn what volumes, machines and kinds exist.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "file_locations",
            "title": "Where files are",
            "description": "Where matching files live: the folders holding them, grouped by \
                machine and volume, with a file count and total size per folder, biggest \
                first. Best for questions like 'where are all my books?' or 'where are my \
                photos from Italy?'. Also returns the total number of matches.",
            "inputSchema": {
                "type": "object",
                "properties": with_limit(FOLDERS_DEFAULT_LIMIT, SEARCH_MAX_LIMIT),
            },
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "search_files",
            "title": "Search files",
            "description": "List individual files matching words in their name or path, \
                optionally limited to a kind, volume or machine. Results are ranked by \
                relevance when words are given. Each result has the machine, volume, \
                absolute path, kind, size in bytes and modification time (unix seconds), and \
                once fingerprinted, `copies` (files with the same content, a mirror file \
                once) and `places` (machines or mirrors holding them; 1 means no backup). \
                Also returns the total number of matches, which can exceed the limit.",
            "inputSchema": {
                "type": "object",
                "properties": with_limit(SEARCH_DEFAULT_LIMIT, SEARCH_MAX_LIMIT),
            },
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "find_duplicates",
            "title": "Find duplicates",
            "description": "Groups of files with the same content in more than one place, \
                most wasted space first. Each group has its size, kind, wasted bytes, whether \
                full hashes confirm it, and each copy's locations (a mirror file is one copy \
                with its replicas). A group matches the filters when any copy does. Also \
                returns the total number of groups.",
            "inputSchema": {"type": "object", "properties": content_filters},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "backup_coverage",
            "title": "Backup coverage",
            "description": "Which files have no copy on another machine. Returns totals \
                (covered, probably covered, uncovered, not yet fingerprinted) and the folders \
                with the most uncovered bytes first. A file is covered when its content is in \
                at least two places on at least two machines; a mirror is one place. Pass \
                a folder from the result to list its files.",
            "inputSchema": {"type": "object", "properties": coverage_props},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "mirror_health",
            "title": "Mirror health",
            "description": "For each configured mirror: its replicas (file counts, newest \
                file, last scan), files only in some replicas, files at the same path with \
                different content, conflict copies, files the sync tools named \
                differently, and files changed after another replica's last scan \
                (not_yet_seen: probably fine, the catalog just hasn't seen the other side \
                yet). Lists are capped with totals. Also suggests folder pairs that \
                look like unconfigured mirrors, with a config snippet the user can add.",
            "inputSchema": {"type": "object", "properties": {
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": SEARCH_MAX_LIMIT,
                    "description": format!("Most items in each list (default {MIRROR_LIST_LIMIT}, \
                        max {SEARCH_MAX_LIMIT}); totals are always complete.")
                }
            }},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "folder_overlaps",
            "title": "Folder overlaps",
            "description": "Pairs of folders on one machine that hold the same files, most \
                shared bytes first: where duplicate space is. Two copies pair at the first \
                folder where their paths differ; a folder paired with itself holds copies \
                inside it. Shared counts each content once. Only files a cleanup plan could \
                remove count (no mirrors, repositories or empty files).",
            "inputSchema": {"type": "object", "properties": {
                "machine": {"type": "string", "description": "Only this machine."},
                "limit": limit_prop(FOLDERS_DEFAULT_LIMIT),
            }},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "drain_preview",
            "title": "Drain preview",
            "description": "What emptying a folder of its duplicates would do, without saving \
                anything: how much it would remove (files with another copy on the same \
                machine), which folders hold the copies that stay, and what it would leave \
                and why (unique, other copies only on other machines, in a mirror, not fingerprinted).",
            "inputSchema": {"type": "object", "required": ["folder"], "properties": {
                "folder": {"type": "string", "description": "machine:volume/path, as \
                    folder_overlaps returns them."},
                "except": except_prop(),
                "limit": limit_prop(FOLDERS_DEFAULT_LIMIT),
            }},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "draft_cleanup_plan",
            "title": "Draft a cleanup plan",
            "description": "Saves a draft cleanup plan in the catalog for the user to review \
                and apply; it moves no files. Either drain (folders to empty of every file \
                with a copy elsewhere on the machine; when all copies are inside them, one \
                stays) or keep + over (remove from `over` each file `keep` also holds). \
                Returns the plan with its id. Tell the user to review it with `gfiles plan \
                show <id>` and apply it themselves with `gfiles plan apply <id>`, which asks \
                them to type the id; never run it for them. Plans refuse mirrors and folders \
                that look synced.",
            "inputSchema": {"type": "object", "properties": {
                "drain": {"type": "array", "items": {"type": "string"},
                    "description": "Folders to drain, all on one machine (machine:volume/path)."},
                "keep": {"type": "string", "description": "The folder whose copies stay."},
                "over": {"type": "string", "description": "The folder files are removed from."},
                "except": except_prop(),
            }},
            "annotations": {"readOnlyHint": false, "destructiveHint": false,
                "idempotentHint": false},
        },
        {
            "name": "draft_plan_from_list",
            "title": "Draft a plan from a list of moves and removals",
            "description": "Saves a draft plan from a list you worked out, for the user to \
                review and apply; it moves no files. Each entry is either {\"remove\": file, \
                \"keep\": file} (remove a file, naming the copy that stays) or {\"move\": file, \
                \"to\": path} (move a file to a path that doesn't exist yet, on the same \
                machine). Files are machine:volume/path, with the machine, volume and path \
                search results give; all entries are on one machine. Add \"reason\" to say why. \
                A removal is checked: unless \"vouched\": true (which needs a reason), the \
                catalog must not show the two files differ, and apply compares their full \
                content. Vouch only when you know they hold the same thing in different bytes. \
                Entries that fail a check are counted in `rejected` and listed, with the \
                reason, in `rejected_entries`; the rest make the plan. Pass `entries` (up to 1000) or, for a longer list, `file`: the \
                path of a JSON-lines file (one entry per line) on this machine. Tell the user \
                to review it with `gfiles plan show <id>` and apply it themselves with \
                `gfiles plan apply <id>`; never run it for them.",
            "inputSchema": {"type": "object", "properties": {
                "entries": {"type": "array", "items": {"type": "object", "properties": {
                    "remove": {"type": "string"}, "keep": {"type": "string"},
                    "move": {"type": "string"}, "to": {"type": "string"},
                    "vouched": {"type": "boolean"}, "reason": {"type": "string"}}},
                    "description": "The moves and removals, in order."},
                "file": {"type": "string",
                    "description": "Absolute path of a JSON-lines list on this machine, \
                        instead of entries."},
            }},
            "annotations": {"readOnlyHint": false, "destructiveHint": false,
                "idempotentHint": false},
        },
        {
            "name": "list_cleanup_plans",
            "title": "List cleanup plans",
            "description": "Every cleanup plan, newest first, with its state (draft, \
                applying, applied, undone, emptied) and totals.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "show_cleanup_plan",
            "title": "Show a cleanup plan",
            "description": "A cleanup plan's totals, the files it removes with the copy that \
                stays for each (and, once applied, whether each moved), and what it leaves: \
                by reason, by folder one level below the plan's folders, and the biggest \
                files. Pass `under` to drill into one folder.",
            "inputSchema": {"type": "object", "required": ["id"], "properties": {
                "id": {"type": "string"},
                "limit": limit_prop(SEARCH_DEFAULT_LIMIT),
                "under": {
                    "type": "string",
                    "description": "A folder inside the plan, as machine:volume/path (like \
                        a left_by_folder entry). Limits the files it removes and the leftovers \
                        to it, and groups the leftovers one level below it. Totals stay the \
                        whole plan's."
                },
            }},
            "annotations": {"readOnlyHint": true},
        },
    ])
}

fn except_prop() -> Value {
    json!({
        "type": "array", "items": {"type": "string"},
        "description": "Folders inside the folder(s) files are removed from to leave out \
            entirely (machine:volume/path): nothing in them is removed or listed."
    })
}

fn limit_prop(default: usize) -> Value {
    json!({
        "type": "integer", "minimum": 1, "maximum": SEARCH_MAX_LIMIT,
        "description": format!("Most items listed (default {default}, max {SEARCH_MAX_LIMIT}).")
    })
}

fn string_arg(args: &Value, key: &str) -> Result<Option<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => anyhow::bail!("{key} must be a string, got {other}"),
    }
}

fn folder_overlaps(conn: &Connection, args: &Value) -> Result<Value> {
    let machine = string_arg(args, "machine")?;
    let limit = limit_from(args, FOLDERS_DEFAULT_LIMIT)?;
    Ok(serde_json::to_value(overlap::pairs(
        conn,
        machine.as_deref(),
        Some(limit),
    )?)?)
}

fn drain_preview(conn: &Connection, args: &Value) -> Result<Value> {
    let folder = string_arg(args, "folder")?
        .ok_or_else(|| anyhow::anyhow!("folder is required (machine:volume/path)"))?;
    let limit = limit_from(args, FOLDERS_DEFAULT_LIMIT)?;
    let folder = mirror::parse_folder(&folder)?;
    let except = folder_list(args, "except")?.unwrap_or_default();
    Ok(serde_json::to_value(overlap::drain_preview(
        conn,
        &folder,
        &except,
        Some(limit),
    )?)?)
}

/// An optional argument holding a list of machine:volume/path folders.
fn folder_list(args: &Value, name: &str) -> Result<Option<Vec<mirror::FolderRef>>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => Ok(Some(
            a.iter()
                .map(|f| {
                    f.as_str()
                        .ok_or_else(|| anyhow::anyhow!("{name} must be a list of folder strings"))
                        .and_then(mirror::parse_folder)
                })
                .collect::<Result<Vec<_>>>()?,
        )),
        Some(other) => anyhow::bail!("{name} must be a list of folders, got {other}"),
    }
}

fn draft_cleanup_plan(conn: &Connection, config: Option<&Config>, args: &Value) -> Result<Value> {
    let drain = folder_list(args, "drain")?;
    let except = folder_list(args, "except")?.unwrap_or_default();
    let keep = string_arg(args, "keep")?;
    let over = string_arg(args, "over")?;
    let spec = match (drain, keep, over) {
        (Some(folders), None, None) if !folders.is_empty() => Spec::Drain(folders),
        (None, Some(keep), Some(over)) => Spec::Keep {
            keeper: mirror::parse_folder(&keep)?,
            over: mirror::parse_folder(&over)?,
        },
        _ => anyhow::bail!("pass either drain (a list of folders) or both keep and over"),
    };
    let built = plan::build(conn, spec, &except)?;
    let not_synced = config
        .map(Config::not_synced_folders)
        .transpose()?
        .unwrap_or_default();
    plan::check_not_synced(conn, &built, &not_synced).map_err(|e| {
        e.context(
            "Only the user knows whether a sync tool manages these folders. Ask them; they \
             edit the hub's config themselves. Don't change the config for them",
        )
    })?;
    let now = crate::scan::unix_secs(Some(std::time::SystemTime::now()));
    let id = plan::save(conn, &built, now)?;
    let mut view = serde_json::to_value(plan::show(conn, &id, Some(SEARCH_DEFAULT_LIMIT), None)?)?;
    view["next_step"] = json!(format!(
        "Saved as a draft; nothing has moved. The user reviews it with `gfiles plan show {id}` \
         and applies it themselves with `gfiles plan apply {id}`, or deletes it with \
         `gfiles plan drop {id}`."
    ));
    Ok(view)
}

fn draft_plan_from_list(conn: &Connection, config: Option<&Config>, args: &Value) -> Result<Value> {
    let list = match (args.get("entries"), string_arg(args, "file")?) {
        (Some(Value::Array(entries)), None) => {
            if entries.len() > LIST_INLINE_MAX {
                anyhow::bail!(
                    "{} entries is more than {LIST_INLINE_MAX} inline; write them to a JSON-lines \
                     file (one entry per line) and pass its path as `file`",
                    entries.len()
                );
            }
            entries
                .iter()
                .map(|e| serde_json::to_string(e).map_err(Into::into))
                .collect::<Result<Vec<_>>>()?
                .join("\n")
        }
        (None, Some(file)) => {
            let path = std::path::Path::new(&file);
            if !path.is_absolute() {
                anyhow::bail!("file must be an absolute path, got {file:?}");
            }
            let reading = |e: std::io::Error| anyhow::anyhow!("reading {file}: {e}");
            if !std::fs::metadata(path).map_err(reading)?.is_file() {
                anyhow::bail!("{file} isn't a regular file");
            }
            // The cap holds even if the file grows, or isn't what its size says, while it's read.
            let mut text = String::new();
            std::io::Read::read_to_string(
                &mut std::io::Read::take(
                    std::fs::File::open(path).map_err(reading)?,
                    LIST_FILE_MAX + 1,
                ),
                &mut text,
            )
            .map_err(reading)?;
            if text.len() as u64 > LIST_FILE_MAX {
                anyhow::bail!("{file} is bigger than {} MB", LIST_FILE_MAX / 1024 / 1024);
            }
            text
        }
        _ => anyhow::bail!("pass either entries (a list) or file (a path), not both"),
    };
    let built = crate::listplan::build(conn, &list)?;
    let not_synced = config
        .map(Config::not_synced_folders)
        .transpose()?
        .unwrap_or_default();
    plan::check_not_synced(conn, &built, &not_synced).map_err(|e| {
        e.context(
            "Only the user knows whether a sync tool manages these folders. Ask them; they \
             edit the hub's config themselves. Don't change the config for them",
        )
    })?;
    let now = crate::scan::unix_secs(Some(std::time::SystemTime::now()));
    let id = plan::save(conn, &built, now)?;
    let mut view = serde_json::to_value(plan::show(conn, &id, Some(SEARCH_DEFAULT_LIMIT), None)?)?;
    view["next_step"] = json!(format!(
        "Saved as a draft; nothing has moved. The user reviews it with `gfiles plan show {id}` \
         and applies it themselves with `gfiles plan apply {id}`, or deletes it with \
         `gfiles plan drop {id}`."
    ));
    Ok(view)
}

fn show_cleanup_plan(conn: &Connection, args: &Value) -> Result<Value> {
    let id = string_arg(args, "id")?.ok_or_else(|| anyhow::anyhow!("id is required"))?;
    let limit = limit_from(args, SEARCH_DEFAULT_LIMIT)?;
    let under = string_arg(args, "under")?
        .map(|u| mirror::parse_folder(&u))
        .transpose()?;
    Ok(serde_json::to_value(plan::show(
        conn,
        &id,
        Some(limit),
        under.as_ref(),
    )?)?)
}

fn catalog_overview(conn: &Connection) -> Result<Value> {
    Ok(json!({"volumes": search::stats(conn)?}))
}

fn file_locations(conn: &Connection, args: &Value) -> Result<Value> {
    let q = query_from(conn, args, FOLDERS_DEFAULT_LIMIT)?;
    let folders = search::folders(conn, &q)?;
    let total = search::count(conn, &q)?;
    Ok(json!({"total_matches": total, "folders": folders}))
}

fn search_files(conn: &Connection, args: &Value) -> Result<Value> {
    let q = query_from(conn, args, SEARCH_DEFAULT_LIMIT)?;
    let files = search::search(conn, &q)?;
    let total = search::count(conn, &q)?;
    Ok(json!({"total_matches": total, "files": files}))
}

fn find_duplicates(conn: &Connection, args: &Value) -> Result<Value> {
    let f = filters_from(conn, args)?;
    Ok(serde_json::to_value(dupes::duplicates(conn, &f)?)?)
}

fn backup_coverage(conn: &Connection, args: &Value) -> Result<Value> {
    let f = filters_from(conn, args)?;
    let folder = match args.get("folder") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(other) => anyhow::bail!("folder must be a string, got {other}"),
    };
    Ok(serde_json::to_value(dupes::coverage(conn, &f, folder)?)?)
}

fn mirror_health(conn: &Connection, args: &Value) -> Result<Value> {
    let limit = limit_from(args, MIRROR_LIST_LIMIT)?;
    Ok(json!({
        "mirrors": mirror::health(conn, limit)?,
        "suggestions": mirror::suggestions(conn)?,
        "warnings": mirror::warnings(conn)?,
    }))
}

/// Filters for the content tools: the search filters (minus words) and a minimum size.
fn filters_from(conn: &Connection, args: &Value) -> Result<Filters> {
    if args.get("query").is_some() {
        anyhow::bail!("this tool takes no query; use kind, volume, machine and min_size");
    }
    let q = query_from(conn, args, GROUPS_DEFAULT_LIMIT)?;
    let min_size = match args.get("min_size") {
        None | Some(Value::Null) => 0,
        Some(v) => v
            .as_i64()
            .filter(|n| *n >= 0)
            .ok_or_else(|| anyhow::anyhow!("min_size must be a whole number of bytes"))?,
    };
    Ok(Filters {
        kind: q.kind,
        volume: q.volume,
        machine: q.machine,
        min_size,
        limit: q.limit,
    })
}

fn limit_from(args: &Value, default_limit: usize) -> Result<usize> {
    Ok(match args.get("limit") {
        None | Some(Value::Null) => default_limit,
        Some(v) => v
            .as_u64()
            .filter(|n| *n >= 1)
            .ok_or_else(|| anyhow::anyhow!("limit must be a positive integer"))?
            .min(SEARCH_MAX_LIMIT as u64) as usize,
    })
}

fn query_from(conn: &Connection, args: &Value, default_limit: usize) -> Result<Query> {
    let text = |key: &str| -> Result<Option<String>> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(other) => anyhow::bail!("{key} must be a string, got {other}"),
        }
    };
    let kind = text("kind")?;
    if let Some(k) = &kind
        && !classify::all_kinds().any(|known| known == k)
    {
        let known: Vec<_> = classify::all_kinds().collect();
        anyhow::bail!("unknown kind {k:?}; use one of {}", known.join(", "));
    }
    let limit = limit_from(args, default_limit)?;
    let q = Query {
        terms: text("query")?.into_iter().collect(),
        kind,
        volume: text("volume")?,
        machine: text("machine")?,
        limit: Some(limit),
    };
    search::check_filters(conn, &q)?;
    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::VolumeConfig, db, scan};
    use std::fs;

    fn catalog() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        for rel in [
            "Books/Tolkien/The Hobbit.epub",
            "Books/Le Guin/A Wizard of Earthsea.mobi",
            "Downloads/Dune.epub",
            "Photos/beach.jpg",
        ] {
            let p = tmp.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, rel).unwrap();
        }
        let mut conn = db::open_in_memory().unwrap();
        let vol = VolumeConfig {
            id: "v1".into(),
            name: "home".into(),
            path: tmp.path().into(),
            ignore: vec![],
        };
        scan::scan_volume(&mut conn, &vol, "gaffer").unwrap();
        (tmp, conn)
    }

    /// Runs a session of requests through `serve` and returns the responses.
    fn session(conn: &Connection, requests: &[Value]) -> Vec<Value> {
        let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
        let mut out = Vec::new();
        serve(conn, None, input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn call(id: i64, tool: &str, args: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": tool, "arguments": args}})
    }

    #[test]
    fn handshake_and_tool_list() {
        let (_tmp, conn) = catalog();
        let out = session(
            &conn,
            &[
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                       "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                                  "clientInfo": {"name": "test", "version": "0"}}}),
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
            ],
        );
        assert_eq!(out.len(), 3, "the notification gets no reply");
        assert_eq!(out[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(out[0]["result"]["serverInfo"]["name"], "gfiles");
        let names: Vec<&str> = out[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "catalog_overview",
                "file_locations",
                "search_files",
                "find_duplicates",
                "backup_coverage",
                "mirror_health",
                "folder_overlaps",
                "drain_preview",
                "draft_cleanup_plan",
                "draft_plan_from_list",
                "list_cleanup_plans",
                "show_cleanup_plan"
            ]
        );
        assert_eq!(out[2]["id"], 3);
    }

    fn call_tool(conn: &Connection, tool: &str, args: Value) -> Value {
        let out = session(
            conn,
            &[json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": {"name": tool, "arguments": args}})],
        );
        out[0]["result"].clone()
    }

    #[test]
    fn duplicates_coverage_and_mirrors() {
        let (_tmp, conn) = catalog();
        // Dune.epub and the Hobbit have different bytes; scanning fingerprinted all four.
        let r = call_tool(&conn, "find_duplicates", json!({}));
        assert_eq!(r["isError"], false, "{r}");
        assert_eq!(r["structuredContent"]["total_groups"], 0);

        let r = call_tool(
            &conn,
            "backup_coverage",
            json!({"kind": "book", "limit": 1}),
        );
        let c = &r["structuredContent"];
        assert_eq!(c["uncovered"]["files"], 3, "{r}");
        assert_eq!(c["total_folders"], 3);
        assert_eq!(c["folders"].as_array().unwrap().len(), 1);
        let folder = c["folders"][0]["folder"].as_str().unwrap().to_string();
        let r = call_tool(&conn, "backup_coverage", json!({"folder": folder}));
        assert_eq!(
            r["structuredContent"]["files"].as_array().unwrap().len(),
            1,
            "{r}"
        );

        let r = call_tool(&conn, "mirror_health", json!({}));
        assert_eq!(r["structuredContent"]["mirrors"], json!([]), "{r}");

        for (tool, args, says) in [
            ("find_duplicates", json!({"min_size": -1}), "min_size"),
            ("find_duplicates", json!({"query": "x"}), "no query"),
            (
                "backup_coverage",
                json!({"machine": "nosuch"}),
                "use one of: gaffer",
            ),
            (
                "backup_coverage",
                json!({"folder": "gaffer:home/nope"}),
                "folder",
            ),
        ] {
            let r = call_tool(&conn, tool, args);
            assert_eq!(r["isError"], true, "{r}");
            assert!(
                r["content"][0]["text"].as_str().unwrap().contains(says),
                "{r}"
            );
        }
    }

    #[test]
    fn a_plan_drafted_from_a_list() {
        use crate::plan::tests::{A, B, put};
        let conn = db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/a.NEF", 10, Some(A)),
                ("todo/new.JPG", 5, Some(B)),
                ("photo/a.NEF", 12, Some(B)),
            ],
        );
        let r = call_tool(
            &conn,
            "draft_plan_from_list",
            json!({"entries": [
                {"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:darren/photo/a.NEF",
                 "vouched": true, "reason": "same shot; XMP added"},
                {"move": "elrond:darren/todo/new.JPG", "to": "elrond:darren/photo/2013/new.JPG"},
                {"move": "elrond:darren/todo/gone.JPG", "to": "elrond:darren/photo/gone.JPG"},
            ]}),
        );
        assert_eq!(r["isError"], false, "{r}");
        let c = &r["structuredContent"];
        assert_eq!(c["kind"], "list", "{r}");
        assert_eq!(c["vouched"]["files"], 1);
        assert_eq!(c["moves"]["files"], 1);
        assert_eq!(c["rejected"], 1, "{r}");
        assert_eq!(c["rejected_entries"][0]["line"], 3);
        assert!(
            c["next_step"]
                .as_str()
                .unwrap()
                .contains("gfiles plan apply")
        );

        for (args, says) in [
            (json!({}), "either entries"),
            (json!({"file": "relative.jsonl"}), "absolute"),
            (json!({"file": "/dev/zero"}), "isn't a regular file"),
            (json!({"entries": vec![json!({}); 1001]}), "JSON-lines file"),
            (
                json!({"entries": [{"move": "elrond:darren/nope", "to": "elrond:darren/x"}]}),
                "no entry of the list is valid",
            ),
        ] {
            let r = call_tool(&conn, "draft_plan_from_list", args);
            assert_eq!(r["isError"], true, "{r}");
            assert!(
                r["content"][0]["text"].as_str().unwrap().contains(says),
                "{says}: {r}"
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("list.jsonl");
        std::fs::write(
            &file,
            "{\"move\": \"elrond:darren/todo/new.JPG\", \"to\": \"elrond:darren/n.JPG\"}\n",
        )
        .unwrap();
        let r = call_tool(
            &conn,
            "draft_plan_from_list",
            json!({"file": file.to_str().unwrap()}),
        );
        assert_eq!(r["structuredContent"]["moves"]["files"], 1, "{r}");
    }

    #[test]
    fn overlaps_preview_and_a_drafted_plan() {
        use crate::plan::tests::{A, put};
        let conn = db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[("todo/a.jpg", 10, Some(A)), ("Photos/a.jpg", 10, Some(A))],
        );
        let r = call_tool(&conn, "folder_overlaps", json!({}));
        assert_eq!(
            r["structuredContent"]["pairs"][0]["b"],
            "elrond:darren/todo"
        );
        let r = call_tool(
            &conn,
            "drain_preview",
            json!({"folder": "elrond:darren/todo"}),
        );
        assert_eq!(r["structuredContent"]["removable"]["files"], 1);

        let r = call_tool(
            &conn,
            "draft_cleanup_plan",
            json!({"keep": "elrond:darren/Photos"}),
        );
        assert_eq!(r["isError"], true);
        let r = call_tool(
            &conn,
            "draft_cleanup_plan",
            json!({"drain": ["elrond:darren/todo"]}),
        );
        assert_eq!(r["isError"], false, "{r}");
        let plan = &r["structuredContent"];
        assert_eq!(plan["state"], "draft");
        assert!(
            plan["next_step"]
                .as_str()
                .unwrap()
                .contains("gfiles plan apply")
        );
        let id = plan["id"].as_str().unwrap();
        let r = call_tool(&conn, "show_cleanup_plan", json!({"id": id}));
        assert_eq!(
            r["structuredContent"]["items"][0]["keep_path"],
            "Photos/a.jpg"
        );
        let r = call_tool(&conn, "list_cleanup_plans", json!({}));
        assert_eq!(r["structuredContent"]["plans"][0]["id"], id);
        // Drafting moved nothing: the catalog still sees both copies.
        let r = call_tool(&conn, "folder_overlaps", json!({}));
        assert_eq!(r["structuredContent"]["pairs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn unknown_protocol_version_gets_the_newest() {
        let (_tmp, conn) = catalog();
        let out = session(
            &conn,
            &[json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                     "params": {"protocolVersion": "1999-01-01"}})],
        );
        assert_eq!(out[0]["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn where_are_my_books() {
        let (_tmp, conn) = catalog();
        let out = session(
            &conn,
            &[
                call(1, "file_locations", json!({"kind": "book"})),
                call(2, "search_files", json!({"query": "hobbit"})),
                call(3, "search_files", json!({"kind": "book", "limit": 1})),
                call(4, "catalog_overview", json!({})),
            ],
        );
        let loc = &out[0]["result"]["structuredContent"];
        assert_eq!(loc["total_matches"], 3);
        let folders: Vec<&str> = loc["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["folder"].as_str().unwrap())
            .collect();
        assert_eq!(folders.len(), 3);
        assert!(folders.contains(&"Books/Tolkien") && folders.contains(&"Downloads"));

        let hobbit = &out[1]["result"]["structuredContent"]["files"][0];
        assert_eq!(hobbit["path"], "Books/Tolkien/The Hobbit.epub");
        assert_eq!(hobbit["machine"], "gaffer");

        let limited = &out[2]["result"]["structuredContent"];
        assert_eq!(limited["files"].as_array().unwrap().len(), 1);
        assert_eq!(limited["total_matches"], 3);

        let text = out[3]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"volume\":\"home\""), "{text}");
    }

    #[test]
    fn bad_arguments_are_tool_errors_not_protocol_errors() {
        let (_tmp, conn) = catalog();
        let out = session(
            &conn,
            &[
                call(1, "search_files", json!({"kind": "novel"})),
                call(2, "search_files", json!({"limit": 0})),
                call(3, "no_such_tool", json!({})),
                call(4, "file_locations", json!({"machine": "Laptop"})),
                call(5, "search_files", json!({"volume": "Home"})),
            ],
        );
        assert_eq!(out[0]["result"]["isError"], true);
        let msg = out[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("unknown kind"), "{msg}");
        assert_eq!(out[1]["result"]["isError"], true);
        assert_eq!(out[2]["error"]["code"], INVALID_PARAMS);
        for (i, hint) in [(3, "use one of: gaffer"), (4, "use one of: home")] {
            assert_eq!(out[i]["result"]["isError"], true);
            let msg = out[i]["result"]["content"][0]["text"].as_str().unwrap();
            assert!(msg.contains(hint), "{msg}");
        }
    }

    #[test]
    fn malformed_input() {
        let (_tmp, conn) = catalog();
        let mut out = Vec::new();
        let input = "not json\n{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"nope\"}\n\
                     {\"jsonrpc\":\"2.0\",\"id\":6,\"result\":{}}\n";
        serve(&conn, None, input.as_bytes(), &mut out).unwrap();
        let out: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(out.len(), 2, "a response from the client gets no reply");
        assert_eq!(out[0]["error"]["code"], PARSE_ERROR);
        assert_eq!(out[1]["error"]["code"], METHOD_NOT_FOUND);
    }
}
