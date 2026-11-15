//! Synced folders (#27) in the ⋯ panel: "Sync a folder…" picks a folder on
//! this Mac and runs `file.sync.start` (to `/sdcard/Download/<name>`); each
//! sync of the active device shows its state and a Stop button. The
//! daemon's `file.sync.*` events keep the rows current.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Value, json};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use uuid::Uuid;

use crate::app::{Ctx, toast};
use crate::i18n::{tr, trn};
use crate::{SyncVm, WidgetsVm};

/// A `file.sync.list` row as the panel shows it.
pub fn row(v: &Value) -> SyncVm {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let folder = Path::new(s("local"))
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| s("local").to_string());
    let (status, busy, failed) = match (s("state"), v.get("last_error").and_then(Value::as_str)) {
        ("mirroring", _) => (
            trn!("Copying {n} file…", "Copying {n} files…", n("files")),
            true,
            false,
        ),
        ("syncing", _) => (tr!("Copying changes…"), true, false),
        (_, Some(e)) => (e.to_string(), false, true),
        _ => (
            trn!(
                "Up to date · {n} file copied",
                "Up to date · {n} files copied",
                n("pushed")
            ),
            false,
            false,
        ),
    };
    SyncVm {
        id: s("id").into(),
        folder: folder.into(),
        remote: s("remote").into(),
        status: status.into(),
        busy,
        failed,
    }
}

/// Puts a `file.sync.list` answer into the view model.
pub fn merge(vm: &mut WidgetsVm, list: &Value) {
    let rows: Vec<SyncVm> = list
        .as_array()
        .map(|a| a.iter().map(row).collect())
        .unwrap_or_default();
    vm.syncs = ModelRc::new(VecModel::from(rows));
}

/// A first copy's `file.sync.progress` event: the row counts up. False when
/// no row has the event's id.
pub fn progress(vm: &WidgetsVm, ev: &Value) -> bool {
    let id = ev.get("id").and_then(Value::as_str).unwrap_or_default();
    let n = |k: &str| ev.get(k).and_then(Value::as_u64).unwrap_or(0);
    let Some(i) = vm.syncs.iter().position(|s| s.id == id) else {
        return false;
    };
    let Some(mut r) = vm.syncs.row_data(i) else {
        return false;
    };
    r.status = match ev.get("phase").and_then(Value::as_str) {
        Some("mirror") => tr!("Copying {} of {} files…", n("done"), n("total")),
        _ => tr!("Copying changes…"),
    }
    .into();
    r.busy = true;
    r.failed = false;
    vm.syncs.set_row_data(i, r);
    true
}

/// Reloads the active device's syncs.
pub fn load(ctx: &Rc<Ctx>, device: Uuid) {
    let token = crate::app::defer(move |r: jidr_core::JidrResult<Value>| {
        let Some(c) = crate::app::ctx() else { return };
        match r {
            Ok(v) => {
                let mut vm = c.app.get_widgets();
                merge(&mut vm, &v);
                c.app.set_widgets(vm);
            }
            // An older daemon has no `file.sync.*`.
            Err(e) => tracing::debug!("file.sync.list: {}", e.message),
        }
    });
    ctx.bridge
        .call("file.sync.list", json!({ "device": device }), move |r| {
            crate::app::fire(token, r)
        });
}

/// A `file.sync.*` event about the active device (UI thread).
pub fn on_event(ctx: &Rc<Ctx>, device: Uuid, topic: &str, data: &Value) {
    if data.get("device").and_then(Value::as_str) != Some(device.to_string().as_str()) {
        return;
    }
    if topic == "file.sync.progress" && progress(&ctx.app.get_widgets(), data) {
        return;
    }
    if topic == "file.sync.error"
        && let Some(e) = data.get("error").and_then(Value::as_str)
    {
        toast(&ctx.app.as_weak(), e.to_string());
    }
    load(ctx, device);
}

/// Synced-folder actions; true when handled.
pub fn action(ctx: &Rc<Ctx>, device: Uuid, action: &str, value: &str) -> bool {
    match action {
        "sync.choose" => {
            let running = ctx
                .info(device)
                .is_some_and(|d| matches!(d.device.status, jidr_core::DeviceStatus::Running));
            if !running {
                toast(
                    &ctx.app.as_weak(),
                    tr!("Start the device first, then sync the folder"),
                );
                return true;
            }
            let token = crate::app::defer(move |path: Option<PathBuf>| {
                if let (Some(p), Some(c)) = (path, crate::app::ctx()) {
                    start(&c, device, p);
                }
            });
            std::thread::spawn(move || crate::app::fire(token, choose_folder()));
        }
        "sync.stop" => {
            let token = crate::app::defer(move |r: jidr_core::JidrResult<Value>| {
                let Some(c) = crate::app::ctx() else { return };
                if let Err(e) = r {
                    toast(&c.app.as_weak(), e.message);
                }
                load(&c, device);
            });
            ctx.bridge
                .call("file.sync.stop", json!({ "id": value }), move |r| {
                    crate::app::fire(token, r)
                });
        }
        _ => return false,
    }
    true
}

fn start(ctx: &Rc<Ctx>, device: Uuid, folder: PathBuf) {
    let token = crate::app::defer(move |r: jidr_core::JidrResult<Value>| {
        let Some(c) = crate::app::ctx() else { return };
        match r {
            Ok(v) => toast(
                &c.app.as_weak(),
                tr!(
                    "Syncing {} to {}",
                    row(&v).folder,
                    v.get("remote").and_then(Value::as_str).unwrap_or_default()
                ),
            ),
            Err(e) => toast(&c.app.as_weak(), e.message),
        }
        load(&c, device);
    });
    ctx.bridge.call(
        "file.sync.start",
        json!({ "device": device, "local": folder }),
        move |r| crate::app::fire(token, r),
    );
}

/// Asks for a folder with the macOS chooser.
fn choose_folder() -> Option<PathBuf> {
    let prompt =
        crate::i18n::applescript_string(&tr!("Choose a folder to keep copied to the device"));
    let out = std::process::Command::new("osascript")
        .args([
            "-e",
            &format!("POSIX path of (choose folder with prompt {prompt})"),
        ])
        .output()
        .ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then(|| PathBuf::from(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(state: &str, error: Option<&str>) -> Value {
        json!({
            "id": "4b0c1a2e-0000-0000-0000-000000000000",
            "local": "/Users/me/site/dist",
            "remote": "/sdcard/Download/dist",
            "state": state,
            "files": 120,
            "pushed": 1,
            "last_error": error,
        })
    }

    #[test]
    fn rows_say_what_a_sync_is_doing() {
        let r = row(&sync("mirroring", None));
        assert_eq!(
            (r.folder.as_str(), r.remote.as_str()),
            ("dist", "/sdcard/Download/dist")
        );
        assert_eq!(r.status, "Copying 120 files…");
        assert!(r.busy && !r.failed);
        let r = row(&sync("watching", None));
        assert_eq!(r.status, "Up to date · 1 file copied");
        assert!(!r.busy && !r.failed);
        let r = row(&sync("watching", Some("adb push failed: no space")));
        assert_eq!(r.status, "adb push failed: no space");
        assert!(r.failed);
        assert_eq!(row(&sync("syncing", None)).status, "Copying changes…");
    }

    #[test]
    fn progress_counts_up_the_matching_row() {
        let mut vm = WidgetsVm::default();
        merge(&mut vm, &json!([sync("mirroring", None)]));
        assert_eq!(vm.syncs.row_count(), 1);
        let ev = json!({ "id": "4b0c1a2e-0000-0000-0000-000000000000", "phase": "mirror", "done": 40, "total": 120 });
        assert!(progress(&vm, &ev));
        assert_eq!(
            vm.syncs.row_data(0).unwrap().status,
            "Copying 40 of 120 files…"
        );
        assert!(!progress(
            &vm,
            &json!({ "id": "other", "done": 1, "total": 2 })
        ));
        merge(&mut vm, &json!([]));
        assert_eq!(vm.syncs.row_count(), 0);
    }
}
