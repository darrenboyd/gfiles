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
    let hits: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let hits = hits.as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["volume"], "Books");
    assert_eq!(hits[0]["path"], "Tolkien/The Hobbit.epub");

    gfiles()
        .args(["search", "--kind", "nope"])
        .assert()
        .failure();
    gfiles()
        .args(["volume", "remove", "Books"])
        .assert()
        .success();
    let out = gfiles().args(["stats", "--json"]).output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "[]");
}
