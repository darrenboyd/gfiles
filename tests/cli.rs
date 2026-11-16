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

/// `sync` on one machine pushes through `ssh` into another catalog. A fake `ssh` on PATH runs
/// the remote command locally, so the real CLI paths run end to end.
#[cfg(unix)]
#[test]
fn sync_pushes_to_central() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let books = tmp.path().join("Books");
    fs::create_dir_all(&books).unwrap();
    fs::write(books.join("Dune.epub"), "x").unwrap();

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
    let central_cmd = format!(
        "{} --db {} --config {}",
        gfiles_bin.display(),
        tmp.path().join("central.db").display(),
        tmp.path().join("central.toml").display()
    );
    fs::write(
        tmp.path().join("agent.toml"),
        format!("machine = \"gaffer\"\n[central]\nhost = \"bill\"\ngfiles = \"{central_cmd}\"\n"),
    )
    .unwrap();
    fs::write(tmp.path().join("central.toml"), "machine = \"bill\"\n").unwrap();

    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let agent = || {
        let mut cmd = Command::cargo_bin("gfiles").unwrap();
        cmd.env("GFILES_CONFIG", tmp.path().join("agent.toml"))
            .env("GFILES_DB", tmp.path().join("agent.db"))
            .env("PATH", &path);
        cmd
    };
    agent()
        .args(["volume", "add"])
        .arg(&books)
        .assert()
        .success();
    let out = agent().arg("sync").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("Pushed to bill"), "{stdout}");
    assert!(stdout.contains("Books: 1 entries, 0 removed"), "{stdout}");

    let out = Command::cargo_bin("gfiles")
        .unwrap()
        .args(["--db"])
        .arg(tmp.path().join("central.db"))
        .args(["search", "dune", "--json"])
        .output()
        .unwrap();
    let hits: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(hits["files"][0]["machine"], "gaffer");
    assert_eq!(hits["files"][0]["path"], "Dune.epub");
}
