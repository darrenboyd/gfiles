//! `gfiles mcp`: a Model Context Protocol server on stdin/stdout, so AI tools can query the
//! catalog. Works over SSH too: `ssh bill gfiles mcp`.
//!
//! The protocol layer is written by hand: newline-delimited JSON-RPC 2.0 with just the methods
//! a tools-only server needs (`initialize`, `ping`, `tools/list`, `tools/call`). That keeps the
//! crate synchronous, with no async runtime. Stdout carries protocol messages only; anything
//! else goes to stderr. Every tool is read-only.

use std::io::{BufRead, Write};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::classify;
use crate::search::{self, Query};

/// Protocol versions this server can speak; it answers with the client's if listed, else the
/// newest. Tools-only servers behave the same under all of them.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const SEARCH_DEFAULT_LIMIT: usize = 50;
const SEARCH_MAX_LIMIT: usize = 500;
const FOLDERS_DEFAULT_LIMIT: usize = 30;

const INSTRUCTIONS: &str = "\
gfiles is a catalog of the user's own files (not system files) across all their machines. \
Each file belongs to a volume (a scanned folder) on a machine, and has a kind: book, photo, \
music, video, document, archive, code, repo (a whole git repository) or other. \
Kinds come from file extensions, so PDFs are 'document' even when they are books. \
Start with catalog_overview to see what exists. For 'where are my X?' questions use \
file_locations, which groups matches by folder; use search_files to list specific files. \
Paths are absolute on the machine named in each result, which may not be this one.";

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

/// Serves requests from `input` until it closes.
pub fn serve(conn: &Connection, input: impl BufRead, mut output: impl Write) -> Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(conn, &line) {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// One message in, at most one response out. Notifications and responses get no reply.
fn handle_line(conn: &Connection, line: &str) -> Option<Value> {
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
    Some(match handle_request(conn, method, &params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(e) => error_response(id, e),
    })
}

fn error_response(id: Value, e: RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.code, "message": e.message}})
}

fn handle_request(conn: &Connection, method: &str, params: &Value) -> Result<Value, RpcError> {
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
            let result = match name {
                "catalog_overview" => catalog_overview(conn),
                "file_locations" => file_locations(conn, &args),
                "search_files" => search_files(conn, &args),
                _ => {
                    return Err(RpcError::new(
                        INVALID_PARAMS,
                        format!("unknown tool {name:?}"),
                    ));
                }
            };
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
                absolute path, kind, size in bytes and modification time (unix seconds). \
                Also returns the total number of matches, which can exceed the limit.",
            "inputSchema": {
                "type": "object",
                "properties": with_limit(SEARCH_DEFAULT_LIMIT, SEARCH_MAX_LIMIT),
            },
            "annotations": {"readOnlyHint": true},
        },
    ])
}

fn catalog_overview(conn: &Connection) -> Result<Value> {
    Ok(json!({"volumes": search::stats(conn)?}))
}

fn file_locations(conn: &Connection, args: &Value) -> Result<Value> {
    let q = query_from(args, FOLDERS_DEFAULT_LIMIT)?;
    let folders = search::folders(conn, &q)?;
    let total = search::count(conn, &q)?;
    Ok(json!({"total_matches": total, "folders": folders}))
}

fn search_files(conn: &Connection, args: &Value) -> Result<Value> {
    let q = query_from(args, SEARCH_DEFAULT_LIMIT)?;
    let files = search::search(conn, &q)?;
    let total = search::count(conn, &q)?;
    Ok(json!({"total_matches": total, "files": files}))
}

fn query_from(args: &Value, default_limit: usize) -> Result<Query> {
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
    let limit = match args.get("limit") {
        None | Some(Value::Null) => default_limit,
        Some(v) => v
            .as_u64()
            .filter(|n| *n >= 1)
            .ok_or_else(|| anyhow::anyhow!("limit must be a positive integer"))?
            .min(SEARCH_MAX_LIMIT as u64) as usize,
    };
    Ok(Query {
        terms: text("query")?.into_iter().collect(),
        kind,
        volume: text("volume")?,
        machine: text("machine")?,
        limit: Some(limit),
    })
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
        serve(conn, input.as_bytes(), &mut out).unwrap();
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
            vec!["catalog_overview", "file_locations", "search_files"]
        );
        assert_eq!(out[2]["id"], 3);
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
            ],
        );
        assert_eq!(out[0]["result"]["isError"], true);
        let msg = out[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("unknown kind"), "{msg}");
        assert_eq!(out[1]["result"]["isError"], true);
        assert_eq!(out[2]["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn malformed_input() {
        let (_tmp, conn) = catalog();
        let mut out = Vec::new();
        let input = "not json\n{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"nope\"}\n\
                     {\"jsonrpc\":\"2.0\",\"id\":6,\"result\":{}}\n";
        serve(&conn, input.as_bytes(), &mut out).unwrap();
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
