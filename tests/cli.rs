//! End-to-end: add a volume, scan it, and search it through the binary.

use std::fs;

use assert_cmd::Command;

#[test]
fn add_scan_search() {
    let tmp = tempfile::tempdir().unwrap();
    let books = tmp.path().join("Books");
    fs::create_dir_all(books.join("Tolkien")).unwrap();
    fs::write(books.join("Tolkien/The Hobbit.epub"), "x").unwrap();
    fs::write(books.join("todo.txt"), "x").unwrap();

    let gfiles = || {
        let mut cmd = Command::cargo_bin("gfiles").unwrap();
        cmd.env("GFILES_CONFIG", tmp.path().join("config.toml"))
            .env("GFILES_DB", tmp.path().join("catalog.db"));
        cmd
    };

    gfiles()
        .args(["volume", "add"])
        .arg(&books)
        .assert()
        .success();
    gfiles().arg("scan").assert().success();

    let out = gfiles()
        .args(["search", "hobbit", "--kind", "book", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let found: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(found["total_matches"], 1);
    let hits = found["files"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["volume"], "Books");
    assert_eq!(hits[0]["path"], "Tolkien/The Hobbit.epub");

    let out = gfiles()
        .args(["search", "-n", "1", "--json"])
        .output()
        .unwrap();
    let found: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        found["total_matches"], 2,
        "the total shows the list was cut off"
    );
    assert_eq!(found["files"].as_array().unwrap().len(), 1);

    gfiles()
        .args(["search", "--kind", "nope"])
        .assert()
        .failure();
    let out = gfiles()
        .args(["search", "--machine", "nope"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown machine"));
    gfiles()
        .args(["volume", "remove", "Books"])
        .assert()
        .success();
    let out = gfiles().args(["stats", "--json"]).output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "[]");
}

/// `sync` on the hub scans locally, then pulls a remote's catalog through `ssh`. A fake `ssh`
/// on PATH runs the remote command locally, so the real CLI paths run end to end.
#[cfg(unix)]
#[test]
fn sync_pulls_from_a_remote() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let books = tmp.path().join("Books");
    fs::create_dir_all(&books).unwrap();
    fs::write(books.join("Dune.epub"), "x").unwrap();
    let notes = tmp.path().join("Notes");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("todo.txt"), "x").unwrap();

    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    // Called as: ssh -o <option>... <host> "<remote command>"; runs the last argument.
    fs::write(
        bin.join("ssh"),
        "#!/bin/sh\nfor arg; do cmd=$arg; done\nexec sh -c \"$cmd\"\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();

    let gfiles_bin = assert_cmd::cargo::cargo_bin("gfiles");
    let nas = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("gfiles").unwrap();
        cmd.env("GFILES_CONFIG", tmp.path().join("nas.toml"))
            .env("GFILES_DB", tmp.path().join("nas.db"))
            .args(args);
        cmd
    };
    fs::write(tmp.path().join("nas.toml"), "machine = \"elrond\"\n").unwrap();
    nas(&["volume", "add"]).arg(&books).assert().success();
    nas(&["scan"]).assert().success();

    let nas_cmd = format!(
        "{} --db {} --config {}",
        gfiles_bin.display(),
        tmp.path().join("nas.db").display(),
        tmp.path().join("nas.toml").display()
    );
    fs::write(
        tmp.path().join("hub.toml"),
        format!("machine = \"gaffer\"\n[[remote]]\nname = \"elrond\"\ngfiles = \"{nas_cmd}\"\n"),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let hub = || {
        let mut cmd = Command::cargo_bin("gfiles").unwrap();
        cmd.env("GFILES_CONFIG", tmp.path().join("hub.toml"))
            .env("GFILES_DB", tmp.path().join("hub.db"))
            .env("PATH", &path);
        cmd
    };
    hub().args(["volume", "add"]).arg(&notes).assert().success();
    let out = hub().arg("sync").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("Notes: 1 files"), "{stdout}");
    assert!(stdout.contains("Pulled elrond"), "{stdout}");
    assert!(stdout.contains("Books: 1 entries, 0 removed"), "{stdout}");

    let out = hub().args(["search", "--json"]).output().unwrap();
    let found: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut seen: Vec<String> = found["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            format!(
                "{}:{}",
                h["machine"].as_str().unwrap(),
                h["path"].as_str().unwrap()
            )
        })
        .collect();
    seen.sort();
    assert_eq!(seen, vec!["elrond:Dune.epub", "gaffer:todo.txt"]);

    // Both files hold the same bytes, and the fingerprint came across with the pull: one
    // duplicate group, and each file has a copy on the other machine.
    let out = hub().args(["dupes", "--json"]).output().unwrap();
    let d: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(d["total_groups"], 1, "{d}");
    assert_eq!(d["groups"][0]["copies"].as_array().unwrap().len(), 2);
    let out = hub().args(["coverage", "--json"]).output().unwrap();
    let c: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(c["uncovered"]["files"], 0, "{c}");
    assert_eq!(c["probably_covered"]["files"], 2, "{c}");
    let out = hub()
        .args(["dupes", "--machine", "bilbo"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown machine"));

    // Taking the remote out of the config forgets its volumes; the hub's own stay.
    let config = fs::read_to_string(tmp.path().join("hub.toml")).unwrap();
    let start = config.find("[[remote]]").unwrap();
    let end = start + config[start..].find("\n\n").unwrap_or(config.len() - start);
    fs::write(
        tmp.path().join("hub.toml"),
        format!("{}{}", &config[..start], &config[end..]),
    )
    .unwrap();
    let out = hub().arg("pull").output().unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("elrond:Books: elrond is no longer a remote")
    );
    let out = hub().args(["search", "--json"]).output().unwrap();
    let found: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(found["total_matches"], 1);
}
