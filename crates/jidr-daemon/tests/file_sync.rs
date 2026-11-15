//! Synced folders (#27) over the socket: the fake backend, a real folder
//! watcher, and a fake adb that logs what it's asked to do.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Conn, Harness};
use serde_json::{Value, json};

/// Replaces the fake SDK's adb with one that logs its arguments.
fn logging_adb(h: &Harness) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let log = h.tmp.path().join("adb.log");
    let adb = h.tmp.path().join("studio/platform-tools/adb");
    std::fs::write(
        &adb,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\necho ok\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    log
}

fn adb_log(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

/// Reads events until `pick` returns something, for at most 15 s.
async fn wait_for<T>(sub: &mut Conn, mut pick: impl FnMut(&str, &Value) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let v = sub.read().await;
            if v["method"] == "event"
                && let Some(t) = pick(
                    v["params"]["topic"].as_str().unwrap_or_default(),
                    &v["params"]["data"],
                )
            {
                return t;
            }
        }
    })
    .await
    .expect("event never came")
}

/// Waits until a `file.sync.synced` of sync `id` satisfies `ok`.
async fn synced(sub: &mut Conn, id: &Value, ok: impl Fn(&Value) -> bool) -> Value {
    wait_for(sub, |topic, d| {
        (topic == "file.sync.synced" && &d["id"] == id && ok(d)).then(|| d.clone())
    })
    .await
}

async fn running_device(c: &mut Conn, name: &str) {
    c.ok("device.create", json!({ "name": name, "image": "35" }))
        .await;
    c.run("device.start", json!({ "device": name })).await;
}

fn folder(h: &Harness) -> PathBuf {
    let dir = h.tmp.path().join("site");
    for (f, body) in [
        ("index.html", "<h1>hi</h1>"),
        ("css/app.css", "body{}"),
        ("node_modules/left-pad/index.js", "x"),
        (".git/HEAD", "ref"),
    ] {
        let p = dir.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    std::fs::canonicalize(dir).unwrap()
}

#[tokio::test]
async fn a_sync_mirrors_the_folder_then_each_change_until_the_device_stops() {
    let h = Harness::start().await;
    let log = logging_adb(&h);
    let mut c = h.client().await;
    let mut sub = h.client().await;
    sub.ok("events.subscribe", json!({ "topics": ["file.sync."] }))
        .await;
    running_device(&mut c, "P").await;
    let dir = folder(&h);

    let s = c
        .ok("file.sync.start", json!({ "device": "P", "local": dir }))
        .await;
    let id = s["id"].clone();
    assert_eq!(s["remote"], "/sdcard/Download/site");
    assert_eq!(s["files"], 2, ".git and node_modules are skipped: {s}");
    assert_eq!(s["ignore"], json!([".DS_Store", ".git", "node_modules"]));
    assert_eq!(s["state"], "mirroring");

    let first = synced(&mut sub, &id, |_| true).await;
    assert_eq!(first["pushed"], 2, "{first}");
    let l = adb_log(&log);
    assert!(
        l.contains("shell set -e; mkdir -p '/sdcard/Download/site'"),
        "{l}"
    );
    assert!(
        l.contains(&format!(
            "push {} {} /sdcard/Download/site/",
            dir.join("css").display(),
            dir.join("index.html").display()
        )),
        "the clean tree goes in one push: {l}"
    );
    assert!(!l.contains("node_modules") && !l.contains(".git"), "{l}");

    // A change goes out alone.
    std::fs::write(dir.join("css/app.css"), "body{color:red}").unwrap();
    synced(&mut sub, &id, |d| d["pushed"] == 1).await;
    let l = adb_log(&log);
    assert!(
        l.contains(&format!(
            "push {} /sdcard/Download/site/css/",
            dir.join("css/app.css").display()
        )),
        "{l}"
    );

    // Deleting on the host deletes on the device.
    std::fs::remove_file(dir.join("index.html")).unwrap();
    synced(&mut sub, &id, |d| d["removed"] == 1).await;
    assert!(
        adb_log(&log).contains("rm -rf '/sdcard/Download/site/index.html'"),
        "{}",
        adb_log(&log)
    );

    // Changes under a skipped folder never reach the device.
    std::fs::write(dir.join("node_modules/left-pad/index.js"), "y").unwrap();

    let list = c.ok("file.sync.list", json!({ "device": "P" })).await;
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["id"], id);
    assert!(list[0]["pushed"].as_u64().unwrap() >= 3, "{list}");
    assert!(list[0]["last_error"].is_null(), "{list}");

    // One sync per device folder.
    let e = c
        .err(
            "file.sync.start",
            json!({ "device": "P", "local": dir, "remote": "/sdcard/Download/site/css" }),
        )
        .await;
    assert_eq!(e["data"]["code"], "usage", "{e}");

    // Stopping the device ends the sync.
    c.ok("device.stop", json!({ "device": "P" })).await;
    let reason = wait_for(&mut sub, |topic, d| {
        (topic == "file.sync.stopped" && d["id"] == id).then(|| d["reason"].clone())
    })
    .await;
    assert_eq!(reason, "device_stopped");
    assert_eq!(c.ok("file.sync.list", json!({})).await, json!([]));
    assert!(!adb_log(&log).contains("node_modules"));
    h.stop().await;
}

#[tokio::test]
async fn stop_takes_an_id_prefix() {
    let h = Harness::start().await;
    logging_adb(&h);
    let mut c = h.client().await;
    running_device(&mut c, "P").await;
    let dir = folder(&h);
    let s = c
        .ok(
            "file.sync.start",
            json!({ "device": "P", "local": dir, "remote": "/sdcard/www/" }),
        )
        .await;
    assert_eq!(s["remote"], "/sdcard/www");
    let id = s["id"].as_str().unwrap();
    let stopped = c.ok("file.sync.stop", json!({ "id": &id[..8] })).await;
    assert_eq!(stopped["id"], id);
    assert_eq!(c.ok("file.sync.list", json!({})).await, json!([]));
    let e = c.err("file.sync.stop", json!({ "id": id })).await;
    assert_eq!(e["data"]["code"], "not_found", "{e}");
    h.stop().await;
}

#[tokio::test]
async fn bad_starts_are_refused() {
    let h = Harness::start().await;
    logging_adb(&h);
    let mut c = h.client().await;
    let dir = folder(&h);
    c.ok("device.create", json!({ "name": "Off", "image": "35" }))
        .await;
    let e = c
        .err("file.sync.start", json!({ "device": "Off", "local": dir }))
        .await;
    assert_eq!(e["data"]["code"], "not_found", "a stopped device: {e}");

    running_device(&mut c, "P").await;
    for (params, why) in [
        (json!({ "device": "P", "local": "site" }), "a relative path"),
        (
            json!({ "device": "P", "local": dir.join("index.html") }),
            "a file",
        ),
        (
            json!({ "device": "P", "local": dir, "remote": "/" }),
            "the device root",
        ),
        (
            json!({ "device": "P", "local": dir, "max_files": 1 }),
            "too many files",
        ),
    ] {
        let e = c.err("file.sync.start", params).await;
        assert_eq!(e["data"]["code"], "usage", "{why}: {e}");
    }
    // Opting out of the default skips counts them again.
    let e = c
        .err(
            "file.sync.start",
            json!({ "device": "P", "local": dir, "no_default_ignore": true, "max_files": 3 }),
        )
        .await;
    assert!(
        e["message"].as_str().unwrap().contains("more than 3 files"),
        "{e}"
    );
    assert_eq!(c.ok("file.sync.list", json!({})).await, json!([]));
    h.stop().await;
}
