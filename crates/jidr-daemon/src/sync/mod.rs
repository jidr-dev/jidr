//! Synced folders (#27): `file.sync.start`, `file.sync.stop`, `file.sync.list`.
//!
//! One way, host to device. A sync copies the host folder to the device
//! once, then watches it (`notify`: FSEvents on macOS, inotify on Linux)
//! and mirrors each debounced batch of changes with `adb push` and `rm`.
//! Deleting on the host deletes on the device; files that were only on the
//! device are left alone.
//!
//! A sync lives as long as its device runs: when the device stops,
//! hibernates, crashes or is deleted, the sync ends with
//! `file.sync.stopped`. It isn't resumed at the next boot, nor kept across
//! a daemon restart: the folder may have changed a lot meanwhile, and a new
//! `file.sync.start` checks its size again instead of writing to a device
//! that was started for something else.

pub mod plan;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jidr_adb::Adb;
use jidr_core::provider::{CallCtx, CommandProvider, CommandSpec, DaemonContext, parse_params};
use jidr_core::{JidrError, JidrResult};
use notify::Watcher;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::state::{State, lock};
use plan::{Batch, Debouncer, Ignore, Mirror, Op};

/// adb pushes of one batch and the shell scripts.
const ADB_TIMEOUT: Duration = Duration::from_secs(600);

// ---- params and results -------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartParams {
    /// Device name or id (must be running).
    pub device: String,
    /// Folder on this machine (an absolute path).
    pub local: PathBuf,
    /// Folder on the device; default `/sdcard/Download/<the local folder's name>`.
    #[serde(default)]
    pub remote: Option<String>,
    /// More file or folder names to skip at any depth (exact names, not
    /// patterns), besides `.git`, `node_modules` and `.DS_Store`.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Sync `.git`, `node_modules` and `.DS_Store` too.
    #[serde(default)]
    pub no_default_ignore: bool,
    /// Refuse a folder with more files than this, after skips (default 10000).
    #[serde(default)]
    pub max_files: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopParams {
    /// Sync id (`file.sync.list`), or its first characters when they name one sync.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    /// Only this device's syncs (name or id).
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// The first full copy is running.
    Mirroring,
    /// Copying a batch of changes.
    Syncing,
    /// Up to date, waiting for changes.
    Watching,
}

/// A sync, as `file.sync.start` and `file.sync.list` return it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SyncInfo {
    pub id: Uuid,
    pub device: Uuid,
    /// The device's name when the sync started.
    pub name: String,
    pub local: PathBuf,
    pub remote: String,
    pub state: SyncState,
    /// Files in the folder when the sync started, after skips.
    pub files: u64,
    pub bytes: u64,
    /// Names skipped at any depth.
    pub ignore: Vec<String>,
    /// Files copied so far, the first copy included.
    pub pushed: u64,
    /// Paths deleted on the device so far.
    pub removed: u64,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub last_synced_at: Option<DateTime<Utc>>,
    /// The last failure; cleared by the next batch that goes through.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Why a sync ended (`file.sync.stopped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Requested,
    DeviceStopped,
    DeviceDeleted,
    FolderGone,
    WatchFailed,
}

// ---- provider -----------------------------------------------------------

struct Entry {
    info: Arc<Mutex<SyncInfo>>,
    task: JoinHandle<()>,
}

type Syncs = Mutex<HashMap<Uuid, Entry>>;

pub struct SyncProvider {
    state: Arc<State>,
    syncs: Arc<Syncs>,
    listening: AtomicBool,
}

impl SyncProvider {
    pub fn new(state: Arc<State>) -> Self {
        Self {
            state,
            syncs: Arc::new(Mutex::new(HashMap::new())),
            listening: AtomicBool::new(false),
        }
    }

    /// Ends syncs whose device stops or goes away, from the daemon's own
    /// events. Started by the first `file.sync.start`.
    fn listen(&self) {
        if self.listening.swap(true, Ordering::SeqCst) {
            return;
        }
        let rx = self.state.subscribe();
        let syncs = Arc::downgrade(&self.syncs);
        let state = Arc::downgrade(&self.state);
        tokio::spawn(watch_devices(rx, syncs, state));
    }

    async fn start(&self, p: StartParams) -> JidrResult<Value> {
        self.listen();
        let d = self.state.device(&p.device).await?;
        let info = self.state.connect_info(d.id).await?;
        let local = host_folder(&p.local)?;
        let remote = device_folder(p.remote.as_deref(), &local)?;
        taken(&lock(&self.syncs), d.id, &d.name, &remote)?;
        let ignore = Ignore::new(!p.no_default_ignore, &p.ignore);
        let max = p.max_files.unwrap_or(plan::DEFAULT_MAX_FILES);
        let mirror = {
            let (local, ignore) = (local.clone(), ignore.clone());
            tokio::task::spawn_blocking(move || plan::scan(&local, Path::new(""), &ignore, max))
                .await
                .map_err(|e| JidrError::unexpected(format!("scanning the folder: {e}")))??
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |ev| {
            let _ = tx.send(ev);
        })
        .map_err(|e| JidrError::unexpected(format!("can't watch {}: {e}", local.display())))?;
        watcher
            .watch(&local, notify::RecursiveMode::Recursive)
            .map_err(|e| JidrError::unexpected(format!("can't watch {}: {e}", local.display())))?;
        let sync = SyncInfo {
            id: Uuid::new_v4(),
            device: d.id,
            name: d.name.clone(),
            local: local.clone(),
            remote,
            state: SyncState::Mirroring,
            files: mirror.files,
            bytes: mirror.bytes,
            ignore: ignore.names(),
            pushed: 0,
            removed: 0,
            started_at: Utc::now(),
            last_synced_at: None,
            last_error: None,
        };
        let shared = Arc::new(Mutex::new(sync.clone()));
        let run = Run {
            id: sync.id,
            device: d.id,
            root: local,
            remote: sync.remote.clone(),
            ignore,
            max_files: max,
            adb: Adb::new(info.adb_path)
                .device(info.adb_serial)
                .with_timeout(ADB_TIMEOUT),
            info: shared.clone(),
            state: Arc::downgrade(&self.state),
            syncs: Arc::downgrade(&self.syncs),
        };
        {
            // Checked again under the lock: another start may have won
            // while this one scanned.
            let mut syncs = lock(&self.syncs);
            taken(&syncs, d.id, &d.name, &sync.remote)?;
            self.state.emit("file.sync.started", to_value(&sync)?);
            // The watcher lives as long as the task.
            let task = tokio::spawn(async move {
                let _watcher = watcher;
                run.run(mirror, rx).await;
            });
            syncs.insert(sync.id, Entry { info: shared, task });
        }
        // The device may have stopped before the listener saw it.
        if self.state.instance(d.id).is_none() {
            end(&self.syncs, &self.state, sync.id, StopReason::DeviceStopped);
            return Err(JidrError::not_found(format!("{} stopped", d.name)));
        }
        to_value(&sync)
    }

    fn find(&self, sel: &str) -> JidrResult<Uuid> {
        let sel = sel.trim().to_ascii_lowercase();
        let syncs = lock(&self.syncs);
        let hits: Vec<Uuid> = syncs
            .keys()
            .filter(|id| !sel.is_empty() && id.to_string().starts_with(&sel))
            .copied()
            .collect();
        match hits.as_slice() {
            [id] => Ok(*id),
            [] => Err(JidrError::not_found(format!("no sync {sel}"))),
            _ => Err(JidrError::usage(format!(
                "{sel} names {} syncs; give more of the id",
                hits.len()
            ))),
        }
    }

    async fn list(&self, p: ListParams) -> JidrResult<Value> {
        let device = match &p.device {
            Some(sel) => Some(self.state.device(sel).await?.id),
            None => None,
        };
        let mut all: Vec<SyncInfo> = lock(&self.syncs)
            .values()
            .map(|e| lock(&e.info).clone())
            .filter(|s| device.is_none_or(|d| s.device == d))
            .collect();
        all.sort_by_key(|s| s.started_at);
        to_value(&all)
    }
}

#[async_trait]
impl CommandProvider for SyncProvider {
    fn name(&self) -> &str {
        "sync"
    }

    fn commands(&self) -> Vec<CommandSpec> {
        vec![
            CommandSpec::new::<StartParams>(
                "file.sync.start",
                "Keep a folder on this machine copied to a running device: a first full copy, then every change (one way)",
            ),
            CommandSpec::new::<StopParams>("file.sync.stop", "Stop a folder sync"),
            CommandSpec::new::<ListParams>("file.sync.list", "Folder syncs and their state"),
        ]
    }

    async fn call(&self, _ctx: &CallCtx, method: &str, params: Value) -> JidrResult<Value> {
        match method {
            "file.sync.start" => self.start(parse_params(params)?).await,
            "file.sync.stop" => {
                let p: StopParams = parse_params(params)?;
                let id = self.find(&p.id)?;
                let info = end(&self.syncs, &self.state, id, StopReason::Requested)
                    .ok_or_else(|| JidrError::not_found(format!("no sync {id}")))?;
                to_value(&info)
            }
            "file.sync.list" => self.list(parse_params(params)?).await,
            other => Err(JidrError::usage(format!(
                "sync provider has no command {other}"
            ))),
        }
    }
}

fn to_value<T: Serialize>(v: &T) -> JidrResult<Value> {
    serde_json::to_value(v).map_err(|e| JidrError::unexpected(format!("encoding result: {e}")))
}

/// The host folder, canonical (FSEvents reports `/private/var/…` for
/// `/var/…`, so event paths only match a canonical root).
fn host_folder(local: &Path) -> JidrResult<PathBuf> {
    if !local.is_absolute() {
        return Err(JidrError::usage(format!(
            "local must be an absolute path, not {}",
            local.display()
        )));
    }
    let dir = std::fs::canonicalize(local)
        .map_err(|e| JidrError::usage(format!("can't open {}: {e}", local.display())))?;
    if !dir.is_dir() {
        return Err(JidrError::usage(format!(
            "{} is not a folder; use file.push for a file",
            local.display()
        )));
    }
    Ok(dir)
}

/// The device folder: absolute, without `.`/`..` parts or a trailing `/`,
/// and not `/` itself.
pub fn device_folder(remote: Option<&str>, local: &Path) -> JidrResult<String> {
    let remote = match remote {
        Some(r) => r.trim().to_string(),
        None => {
            let name = local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| JidrError::usage("name the device folder with remote"))?;
            format!("/sdcard/Download/{name}")
        }
    };
    let bad = !remote.starts_with('/')
        || remote.contains(['\n', '\r', '\0'])
        || remote.split('/').any(|p| p == "." || p == "..");
    let trimmed = remote.trim_end_matches('/');
    if bad || trimmed.is_empty() {
        return Err(JidrError::usage(format!(
            "remote must be an absolute device folder other than /, without . or .. (got {remote:?})"
        )));
    }
    Ok(trimmed.to_string())
}

/// `usage` when `remote` on `device` is already synced, or holds or sits in
/// a synced folder.
fn taken(syncs: &HashMap<Uuid, Entry>, device: Uuid, name: &str, remote: &str) -> JidrResult<()> {
    let hit = syncs.values().find_map(|e| {
        let s = lock(&e.info);
        (s.device == device && overlaps(&s.remote, remote)).then(|| (s.id, s.remote.clone()))
    });
    match hit {
        Some((id, other)) => Err(JidrError::usage(format!(
            "{other} on {name} is already synced (sync {id}); stop it first"
        ))
        .with_data(json!({ "id": id }))),
        None => Ok(()),
    }
}

/// Whether two device folders are the same or one holds the other.
pub fn overlaps(a: &str, b: &str) -> bool {
    let inside = |x: &str, y: &str| x.strip_prefix(y).is_some_and(|r| r.starts_with('/'));
    a == b || inside(a, b) || inside(b, a)
}

/// Removes a sync, stops its task and announces it. `None` if it was gone.
fn end(syncs: &Syncs, state: &State, id: Uuid, reason: StopReason) -> Option<SyncInfo> {
    let entry = lock(syncs).remove(&id)?;
    entry.task.abort();
    let info = lock(&entry.info).clone();
    state.emit(
        "file.sync.stopped",
        json!({ "id": id, "device": info.device, "reason": reason }),
    );
    Some(info)
}

/// Ends the syncs of devices that stop running (`device.status`) or are
/// deleted. After missed events it checks every sync's device.
async fn watch_devices(
    mut rx: broadcast::Receiver<jidr_core::rpc::Event>,
    syncs: Weak<Syncs>,
    state: Weak<State>,
) {
    loop {
        let ev = rx.recv().await;
        let (Some(syncs), Some(state)) = (syncs.upgrade(), state.upgrade()) else {
            return;
        };
        let device_of = |e: &Entry| lock(&e.info).device;
        let ended: Vec<(Uuid, StopReason)> = match ev {
            Ok(ev) => {
                let reason = match ev.topic.as_str() {
                    "device.status" if ev.data["status"]["state"] != "running" => {
                        StopReason::DeviceStopped
                    }
                    "device.deleted" => StopReason::DeviceDeleted,
                    _ => continue,
                };
                let device = ev.data["device"].as_str().unwrap_or_default().to_string();
                lock(&syncs)
                    .iter()
                    .filter(|(_, e)| device_of(e).to_string() == device)
                    .map(|(id, _)| (*id, reason))
                    .collect()
            }
            Err(broadcast::error::RecvError::Lagged(_)) => lock(&syncs)
                .iter()
                .filter(|(_, e)| state.instance(device_of(e)).is_none())
                .map(|(id, _)| (*id, StopReason::DeviceStopped))
                .collect(),
            Err(broadcast::error::RecvError::Closed) => return,
        };
        for (id, reason) in ended {
            end(&syncs, &state, id, reason);
        }
    }
}

// ---- one sync -----------------------------------------------------------

/// Which copy a `file.sync.progress` event is about.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Mirror,
    Change,
}

struct Run {
    id: Uuid,
    device: Uuid,
    root: PathBuf,
    remote: String,
    ignore: Ignore,
    max_files: u64,
    adb: Adb,
    info: Arc<Mutex<SyncInfo>>,
    state: Weak<State>,
    syncs: Weak<Syncs>,
}

impl Run {
    async fn run(
        self,
        mirror: Mirror,
        mut rx: mpsc::UnboundedReceiver<notify::Result<notify::Event>>,
    ) {
        let result = self.mirror(&mirror).await;
        self.finish(result.map(|pushed| (pushed, 0)));
        let mut pending = Debouncer::default();
        loop {
            let due = pending
                .deadline()
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(Ok(ev)) => self.note(&ev, &mut pending),
                    Some(Err(e)) => self.error(&format!("watching {}: {e}", self.root.display())),
                    None => {
                        self.stop(StopReason::WatchFailed);
                        return;
                    }
                },
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(due)) => {
                    if let Some(paths) = pending.take(Instant::now()) {
                        if !self.root.is_dir() {
                            self.stop(StopReason::FolderGone);
                            return;
                        }
                        match self.apply(&paths).await {
                            Ok(None) => {}
                            Ok(Some(done)) => self.finish(Ok(done)),
                            Err(e) => self.finish(Err(e)),
                        }
                    }
                }
            }
        }
    }

    /// Adds an event's paths to the pending batch (a rescan request
    /// re-copies the whole folder).
    fn note(&self, ev: &notify::Event, pending: &mut Debouncer) {
        if matches!(ev.kind, notify::EventKind::Access(_)) {
            return;
        }
        let now = Instant::now();
        if ev.need_rescan() {
            pending.add(PathBuf::new(), now);
        }
        for path in &ev.paths {
            // The folder itself only counts through a rescan: re-copying
            // everything for its own metadata would be a waste.
            if let Ok(rel) = path.strip_prefix(&self.root)
                && !rel.as_os_str().is_empty()
                && !self.ignore.skips(rel)
            {
                pending.add(rel.to_path_buf(), now);
            }
        }
    }

    /// Mirrors a batch of changed paths → (files pushed, paths removed);
    /// `None` when there was nothing to do (a temporary file came and went).
    async fn apply(&self, changed: &BTreeSet<PathBuf>) -> JidrResult<Option<(u64, u64)>> {
        let ops = plan::plan(changed, &self.ignore, |r| plan::probe(&self.root, r));
        if ops.is_empty() {
            return Ok(None);
        }
        self.set_state(SyncState::Syncing);
        let removed = ops.iter().filter(|o| matches!(o, Op::Remove(_))).count() as u64;
        let mut mkdirs = Vec::new();
        let mut batches = plan::file_batches(&self.root, &ops);
        for o in &ops {
            if let Op::PushDir(rel) = o {
                let (root, rel, ignore, max) = (
                    self.root.clone(),
                    rel.clone(),
                    self.ignore.clone(),
                    self.max_files,
                );
                let m = tokio::task::spawn_blocking(move || plan::scan(&root, &rel, &ignore, max))
                    .await
                    .map_err(|e| JidrError::unexpected(format!("scanning: {e}")))??;
                mkdirs.extend(m.mkdirs);
                // A folder event is often only its metadata changing, or a
                // rescan: send just what is newer than the device's copy
                // (adb push keeps modification times).
                batches.extend(m.batches.into_iter().map(|b| Batch {
                    newer_only: true,
                    ..b
                }));
            }
        }
        let prepare = plan::prepare(&self.remote, &ops, &mkdirs);
        let pushed = self.run_batches(&prepare, &batches, Phase::Change).await?;
        Ok(Some((pushed, removed)))
    }

    /// The first copy.
    async fn mirror(&self, m: &Mirror) -> JidrResult<u64> {
        let dirs: Vec<String> = m
            .mkdirs
            .iter()
            .map(|d| plan::remote_path(&self.remote, d))
            .collect();
        let commands = plan::pack("mkdir -p", &dirs, plan::SCRIPT_LIMIT / 2);
        self.run_batches(&commands, &m.batches, Phase::Mirror).await
    }

    /// Runs the shell commands, then the pushes, reporting progress.
    async fn run_batches(
        &self,
        commands: &[String],
        batches: &[Batch],
        phase: Phase,
    ) -> JidrResult<u64> {
        for script in plan::scripts(commands, plan::SCRIPT_LIMIT) {
            let (code, out) = self.adb.shell_status(&script).await?;
            if code != 0 {
                return Err(JidrError::sdk(format!(
                    "preparing {} on the device failed: {}",
                    self.remote,
                    out.trim()
                )));
            }
        }
        let total: u64 = batches.iter().map(|b| b.files).sum();
        let mut done = 0;
        for b in batches {
            let dest = format!("{}/", plan::remote_path(&self.remote, &b.dir));
            for chunk in b.sources.chunks(plan::PUSH_CHUNK) {
                self.adb.push_many(chunk, &dest, b.newer_only).await?;
            }
            done += b.files;
            self.emit(
                "file.sync.progress",
                json!({ "phase": phase, "done": done, "total": total }),
            );
        }
        Ok(done)
    }

    fn set_state(&self, s: SyncState) {
        lock(&self.info).state = s;
    }

    fn finish(&self, result: JidrResult<(u64, u64)>) {
        match result {
            Ok((pushed, removed)) => {
                let mut info = lock(&self.info);
                info.state = SyncState::Watching;
                info.pushed += pushed;
                info.removed += removed;
                info.last_synced_at = Some(Utc::now());
                info.last_error = None;
                drop(info);
                self.emit(
                    "file.sync.synced",
                    json!({ "pushed": pushed, "removed": removed }),
                );
            }
            Err(e) => {
                self.set_state(SyncState::Watching);
                self.error(&e.message);
            }
        }
    }

    /// Reports a failure; the sync keeps watching.
    fn error(&self, message: &str) {
        tracing::warn!(sync = %self.id, "file.sync: {message}");
        lock(&self.info).last_error = Some(message.to_string());
        self.emit("file.sync.error", json!({ "error": message }));
    }

    fn emit(&self, topic: &str, mut data: Value) {
        if let Some(state) = self.state.upgrade() {
            data["id"] = json!(self.id);
            data["device"] = json!(self.device);
            state.emit(topic, data);
        }
    }

    /// Ends this sync from inside its task (the task isn't aborted: it
    /// returns right after).
    fn stop(&self, reason: StopReason) {
        let (Some(syncs), Some(state)) = (self.syncs.upgrade(), self.state.upgrade()) else {
            return;
        };
        if lock(&syncs).remove(&self.id).is_some() {
            state.emit(
                "file.sync.stopped",
                json!({ "id": self.id, "device": self.device, "reason": reason }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_folders_are_absolute_and_tidy() {
        let local = Path::new("/Users/me/My Site");
        assert_eq!(
            device_folder(None, local).unwrap(),
            "/sdcard/Download/My Site"
        );
        assert_eq!(
            device_folder(Some(" /sdcard/www/ "), local).unwrap(),
            "/sdcard/www"
        );
        for bad in [
            "sdcard/x",
            "/",
            "///",
            "/sdcard/../data",
            "/sdcard/./x",
            "/a\nb",
        ] {
            assert!(device_folder(Some(bad), local).is_err(), "{bad:?}");
        }
        assert!(device_folder(None, Path::new("/")).is_err());
    }

    #[test]
    fn overlapping_device_folders() {
        assert!(overlaps("/sdcard/a", "/sdcard/a"));
        assert!(overlaps("/sdcard/a", "/sdcard/a/b"));
        assert!(overlaps("/sdcard/a/b", "/sdcard/a"));
        assert!(!overlaps("/sdcard/a", "/sdcard/ab"));
        assert!(!overlaps("/sdcard/a", "/sdcard/b"));
    }

    #[test]
    fn host_folder_must_be_an_absolute_folder() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("f"), b"x").unwrap();
        assert!(host_folder(Path::new("relative")).is_err());
        assert!(host_folder(&t.path().join("f")).is_err());
        assert!(host_folder(&t.path().join("missing")).is_err());
        assert_eq!(
            host_folder(t.path()).unwrap(),
            std::fs::canonicalize(t.path()).unwrap()
        );
    }
}
