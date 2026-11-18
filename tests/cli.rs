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
        "#!/bin/sh\nfor arg; do cmd=$arg; done\nSSH_CONNECTION=test exec sh -c \"$cmd\"\n",
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

/// Runs gfiles with `args` under a fresh pseudo-terminal, typing `typed` there. Returns stdout.
#[cfg(target_os = "linux")]
fn under_pty(envs: &[(&str, String)], args: &str, typed: &str) -> String {
    let gfiles = assert_cmd::cargo::cargo_bin("gfiles");
    let mut cmd = std::process::Command::new("setsid");
    cmd.args(["-w", "script", "-qec"])
        .arg(format!("{} {args}", gfiles.display()))
        .arg("/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(format!("{typed}\n").as_bytes()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg(target_os = "linux")]
fn plan_id(json: &[u8]) -> String {
    let v: serde_json::Value = serde_json::from_slice(json).unwrap();
    v["id"].as_str().unwrap().to_string()
}

#[test]
#[cfg(target_os = "linux")]
fn plan_apply_undo_and_empty_on_this_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("darren");
    fs::create_dir_all(root.join("todo/2012")).unwrap();
    fs::create_dir_all(root.join("Photos/2012")).unwrap();
    fs::write(root.join("todo/2012/a.jpg"), "photo a").unwrap();
    fs::write(root.join("Photos/2012/a.jpg"), "photo a").unwrap();
    fs::write(root.join("todo/only.jpg"), "unique").unwrap();
    let envs = [
        (
            "GFILES_CONFIG",
            tmp.path().join("config.toml").display().to_string(),
        ),
        (
            "GFILES_DB",
            tmp.path().join("catalog.db").display().to_string(),
        ),
    ];
    let gfiles = || {
        let mut cmd = Command::cargo_bin("gfiles").unwrap();
        cmd.envs(envs.iter().map(|(k, v)| (*k, v.clone())));
        cmd
    };
    fs::write(tmp.path().join("config.toml"), "machine = \"elrond\"\n").unwrap();
    gfiles()
        .args(["volume", "add"])
        .arg(&root)
        .assert()
        .success();
    gfiles().arg("scan").assert().success();

    let out = gfiles().args(["overlaps", "--json"]).output().unwrap();
    let overlaps: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(overlaps["total"], 1);
    let pairs = &overlaps["pairs"];
    assert_eq!(pairs[0]["a"], "elrond:darren/Photos");
    assert_eq!(pairs[0]["b"], "elrond:darren/todo");

    let out = gfiles()
        .args(["plan", "drain", "elrond:darren/todo", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = plan_id(&out.stdout);

    // No terminal: refused, nothing moved.
    let out = std::process::Command::new("setsid")
        .args(["-w"])
        .arg(assert_cmd::cargo::cargo_bin("gfiles"))
        .args(["plan", "apply", &id])
        .envs(envs.iter().map(|(k, v)| (*k, v.clone())))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs you at a terminal"));
    assert!(root.join("todo/2012/a.jpg").exists());

    let out = under_pty(&envs, &format!("plan apply {id}"), "wrong");
    assert!(out.contains("isn't the plan id"), "{out}");
    assert!(root.join("todo/2012/a.jpg").exists());

    let out = under_pty(&envs, &format!("plan apply {id}"), &id);
    assert!(out.contains("Moved 1 files, skipped 0"), "{out}");
    assert!(!root.join("todo/2012/a.jpg").exists());
    assert!(
        root.join(format!(".gfiles-trash/{id}/files/todo/2012/a.jpg"))
            .exists()
    );
    assert!(root.join("todo/only.jpg").exists());

    // A rescan doesn't catalog the trash, and the moved file is gone from the overlaps.
    gfiles().arg("scan").assert().success();
    let out = gfiles().args(["search", "a", "--json"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains(".gfiles-trash"));
    let out = gfiles().args(["overlaps", "--json"]).output().unwrap();
    let overlaps: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(overlaps["pairs"], serde_json::json!([]));

    let out = under_pty(&envs, &format!("plan undo {id}"), &id);
    assert!(out.contains("Put back 1 files"), "{out}");
    assert!(root.join("todo/2012/a.jpg").exists());
    gfiles().args(["plan", "drop", &id]).assert().failure();
    gfiles().arg("scan").assert().success();

    let out = gfiles()
        .args([
            "plan",
            "keep",
            "elrond:darren/Photos",
            "--over",
            "elrond:darren/todo",
            "--json",
        ])
        .output()
        .unwrap();
    let id = plan_id(&out.stdout);
    let out = under_pty(&envs, &format!("plan apply {id}"), &id);
    assert!(out.contains("Moved 1 files"), "{out}");
    let out = under_pty(&envs, &format!("plan empty {id}"), &id);
    assert!(out.contains("Deleted 1 files"), "{out}");
    assert!(!root.join(".gfiles-trash").join(&id).exists());
    let out = gfiles().args(["plan", "list", "--json"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"state\": \"emptied\""));
}

#[test]
#[cfg(target_os = "linux")]
fn a_plan_from_a_list_moves_and_removes_then_undoes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("darren");
    fs::create_dir_all(root.join("todo")).unwrap();
    fs::create_dir_all(root.join("photo/2007")).unwrap();
    fs::write(root.join("todo/a.NEF"), "raw a").unwrap();
    fs::write(root.join("photo/2007/a.NEF"), "raw a, tagged").unwrap();
    fs::write(root.join("todo/new.JPG"), "new photo").unwrap();
    let envs = [
        (
            "GFILES_CONFIG",
            tmp.path().join("config.toml").display().to_string(),
        ),
        (
            "GFILES_DB",
            tmp.path().join("catalog.db").display().to_string(),
        ),
    ];
    let gfiles = || {
        let mut cmd = Command::cargo_bin("gfiles").unwrap();
        cmd.envs(envs.iter().map(|(k, v)| (*k, v.clone())));
        cmd
    };
    fs::write(tmp.path().join("config.toml"), "machine = \"elrond\"\n").unwrap();
    gfiles()
        .args(["volume", "add"])
        .arg(&root)
        .assert()
        .success();
    gfiles().arg("scan").assert().success();

    let list = r#"{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:darren/photo/2007/a.NEF", "vouched": true, "reason": "same shot, tagged"}
{"move": "elrond:darren/todo/new.JPG", "to": "elrond:darren/photo/2013/08/new.JPG", "reason": "taken 2013-08"}
{"move": "elrond:darren/todo/gone.JPG", "to": "elrond:darren/photo/gone.JPG"}
"#;
    let out = gfiles()
        .args(["plan", "import", "-"])
        .write_stdin(list)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("vouched"), "{text}");
    assert!(text.contains("-> /"), "{text}");
    assert!(
        text.contains("line 3: elrond:darren/todo/gone.JPG is not in the catalog"),
        "{text}"
    );
    let out = gfiles().args(["plan", "list", "--json"]).output().unwrap();
    let plans: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = plans[0]["id"].as_str().unwrap().to_string();
    assert_eq!(plans[0]["kind"], "list");

    let out = under_pty(&envs, &format!("plan apply {id}"), &id);
    assert!(out.contains("vouched for"), "{out}");
    assert!(out.contains("Moved 2 files, skipped 0"), "{out}");
    assert!(!root.join("todo/a.NEF").exists());
    assert_eq!(
        fs::read_to_string(root.join("photo/2013/08/new.JPG")).unwrap(),
        "new photo"
    );

    let out = under_pty(&envs, &format!("plan undo {id}"), &id);
    assert!(out.contains("Put back 2 files"), "{out}");
    assert_eq!(
        fs::read_to_string(root.join("todo/a.NEF")).unwrap(),
        "raw a"
    );
    assert_eq!(
        fs::read_to_string(root.join("todo/new.JPG")).unwrap(),
        "new photo"
    );
    assert!(!root.join("photo/2013").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn plan_apply_runs_on_a_remote_over_ssh() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("darren");
    fs::create_dir_all(root.join("todo")).unwrap();
    fs::create_dir_all(root.join("Photos")).unwrap();
    fs::write(root.join("Photos/keep.jpg"), "same").unwrap();
    // Enough items that the request and the replies each overflow a pipe buffer.
    for i in 0..20_000 {
        fs::write(root.join(format!("todo/{i:05}.jpg")), "same").unwrap();
    }

    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("ssh"),
        "#!/bin/sh\nfor arg; do cmd=$arg; done\nSSH_CONNECTION=test exec sh -c \"$cmd\"\n",
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
    nas(&["volume", "add"]).arg(&root).assert().success();
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
    let envs = [
        (
            "GFILES_CONFIG",
            tmp.path().join("hub.toml").display().to_string(),
        ),
        ("GFILES_DB", tmp.path().join("hub.db").display().to_string()),
        (
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        ),
    ];
    let hub = || {
        let mut cmd = Command::cargo_bin("gfiles").unwrap();
        cmd.envs(envs.iter().map(|(k, v)| (*k, v.clone())));
        cmd
    };
    hub().arg("pull").assert().success();
    let out = hub()
        .args(["plan", "drain", "elrond:darren/todo", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = plan_id(&out.stdout);

    // Run directly (not over ssh), plan exec refuses: it would skip the confirmation.
    let out = std::process::Command::new(&gfiles_bin)
        .args(["plan", "exec"])
        .env_remove("SSH_CONNECTION")
        .env("GFILES_CONFIG", tmp.path().join("nas.toml"))
        .env("GFILES_DB", tmp.path().join("nas.db"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("only runs over SSH"));

    let out = under_pty(&envs, &format!("plan apply {id}"), &id);
    assert!(out.contains("Moved 20000 files, skipped 0"), "{out}");
    assert!(fs::read_dir(root.join("todo")).unwrap().next().is_none());
    assert!(root.join("Photos/keep.jpg").exists());
    let out = hub()
        .args(["plan", "show", &id, "--json", "-n", "1"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["state"], "applied");
    assert_eq!(v["moved"]["files"], 20_000);
    // The NAS stored the full hash it read of the kept copy, for the hub's next pull.
    let hash: Option<String> = rusqlite::Connection::open(tmp.path().join("nas.db"))
        .unwrap()
        .query_row(
            "SELECT content_hash FROM entries WHERE path = 'Photos/keep.jpg'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(hash.is_some_and(|h| h.starts_with("b3:")));

    // A list's move runs on the NAS the same way, creating its folders there.
    fs::write(root.join("Photos/new.jpg"), "new").unwrap();
    nas(&["scan"]).assert().success();
    hub().arg("pull").assert().success();
    hub()
        .args(["plan", "import", "-"])
        .write_stdin(
            "{\"move\": \"elrond:darren/Photos/new.jpg\", \"to\": \"elrond:darren/Photos/2013/new.jpg\"}\n",
        )
        .assert()
        .success();
    let out = hub().args(["plan", "list", "--json"]).output().unwrap();
    let plans: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = plans[0]["id"].as_str().unwrap().to_string();
    let out = under_pty(&envs, &format!("plan apply {id}"), &id);
    assert!(out.contains("Moved 1 files, skipped 0"), "{out}");
    assert_eq!(
        fs::read_to_string(root.join("Photos/2013/new.jpg")).unwrap(),
        "new"
    );
}
