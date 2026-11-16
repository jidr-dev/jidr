//! Input for iOS simulators (D50): Jidr's XCTest runner
//! (`helpers/jidr-sim-runner`), built on this Mac on first use and run per
//! simulator with `xcodebuild test-without-building`. The runner serves
//! line-delimited JSON on `127.0.0.1:<port>`, which simulator processes
//! share with the host; the first line carries a random token.
//!
//! The source ships as plain files (`Jidr.app/Contents/Resources/sim-runner`,
//! `sim-runner/` next to the binary in the MCP bundle, the repo folder in
//! development), so there's no simulator binary inside the notarized app.
//! It's copied into the cache and built there, keyed by Xcode's build
//! version and the source's hash.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jidr_core::{JidrError, JidrResult};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::sync::Mutex;

/// Overrides where the runner's source is.
pub const SOURCE_ENV: &str = "JIDR_SIM_RUNNER_DIR";
const PROJECT: &str = "JidrSimRunner.xcodeproj";
/// How long a runner may take to answer its first `ping`.
const START_TIMEOUT: Duration = Duration::from_secs(180);
/// One request's budget (a `tree` of a busy screen takes a few seconds).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A runner unused this long is stopped.
pub const IDLE: Duration = Duration::from_secs(10 * 60);

/// Folders that may hold the runner's source, first match wins.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os(SOURCE_ENV) {
        dirs.push(PathBuf::from(d));
    }
    if let Some(exe_dir) = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        dirs.push(exe_dir.join("../Resources/sim-runner"));
        dirs.push(exe_dir.join("sim-runner"));
        dirs.push(exe_dir.join("../../helpers/jidr-sim-runner"));
        dirs.push(exe_dir.join("../../../helpers/jidr-sim-runner"));
    }
    dirs
}

fn find_source() -> JidrResult<PathBuf> {
    let dirs = search_dirs();
    dirs.iter()
        .find(|d| d.join(PROJECT).is_dir())
        .cloned()
        .ok_or_else(|| {
            JidrError::not_found(format!(
                "the simulator input runner's source isn't installed (looked in {})",
                dirs.iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// A cache key from the source files' contents.
fn source_hash(dir: &Path) -> u64 {
    fn walk(dir: &Path, h: &mut std::collections::hash_map::DefaultHasher) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.starts_with('.') || name == "xcuserdata" {
                continue;
            }
            name.hash(h);
            if p.is_dir() {
                walk(&p, h);
            } else if let Ok(bytes) = std::fs::read(&p) {
                bytes.hash(h);
            }
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    walk(dir, &mut h);
    h.finish()
}

/// Xcode's build version (`27A9275`).
async fn xcode_build() -> JidrResult<String> {
    let out = tokio::process::Command::new("/usr/bin/xcrun")
        .args(["xcodebuild", "-version"])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| JidrError::sdk(format!("cannot run xcodebuild: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let hint = if err.contains("license") {
            "; accept Xcode's licence yourself first (`sudo xcodebuild -license`)"
        } else {
            ""
        };
        return Err(JidrError::sdk(format!(
            "xcodebuild -version failed: {}{hint}",
            err.trim()
        )));
    }
    Ok(text
        .lines()
        .find_map(|l| l.strip_prefix("Build version "))
        .unwrap_or("unknown")
        .trim()
        .to_string())
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dest.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), to)?;
        }
    }
    Ok(())
}

fn find_xctestrun(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir.join("Build/Products"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "xctestrun"))
}

/// Builds the runner once per Xcode and source version; returns the
/// `.xctestrun` to run.
pub async fn build(cache: &Path, report: &(dyn Fn(&str) + Send + Sync)) -> JidrResult<PathBuf> {
    let source = find_source()?;
    let key = format!("{}-{:016x}", xcode_build().await?, source_hash(&source));
    let root = cache.join("sim-runner").join(&key);
    let derived = root.join("derived");
    if let Some(x) = find_xctestrun(&derived) {
        return Ok(x);
    }
    report("Building the simulator input runner (once per Xcode version)");
    let src = root.join("src");
    let _ = std::fs::remove_dir_all(&src);
    copy_tree(&source, &src)
        .map_err(|e| JidrError::unexpected(format!("copying the runner source: {e}")))?;
    let out = tokio::process::Command::new("/usr/bin/xcrun")
        .arg("xcodebuild")
        .arg("build-for-testing")
        .arg("-quiet")
        .arg("-project")
        .arg(src.join(PROJECT))
        .args([
            "-scheme",
            "JidrSimRunner",
            "-sdk",
            "iphonesimulator",
            "-destination",
            "generic/platform=iOS Simulator",
            "-derivedDataPath",
        ])
        .arg(&derived)
        .arg("CODE_SIGN_IDENTITY=-")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| JidrError::sdk(format!("cannot run xcodebuild: {e}")))?;
    if !out.status.success() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let tail: Vec<&str> = text.lines().rev().take(15).collect();
        return Err(JidrError::sdk(format!(
            "building the simulator input runner failed:\n{}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        )));
    }
    find_xctestrun(&derived)
        .ok_or_else(|| JidrError::sdk("xcodebuild built the runner but left no .xctestrun"))
}

/// The runner's view of the screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    /// Points.
    pub width: f64,
    pub height: f64,
    /// Pixels per point.
    pub scale: f64,
}

/// A running runner on one simulator.
pub struct Runner {
    child: Mutex<Option<Child>>,
    conn: Mutex<Conn>,
    pub port: u16,
    last_used: std::sync::Mutex<Instant>,
    screen: std::sync::Mutex<Option<Screen>>,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner").field("port", &self.port).finish()
    }
}

struct Conn {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    next: u64,
}

impl Conn {
    async fn open(port: u16, token: &str) -> JidrResult<Self> {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| JidrError::unexpected(format!("runner connect: {e}")))?;
        let (r, w) = stream.into_split();
        let mut c = Self {
            reader: BufReader::new(r),
            writer: w,
            next: 1,
        };
        c.request("ping", json!({}), Some(token), Duration::from_secs(10))
            .await?;
        Ok(c)
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        token: Option<&str>,
        timeout: Duration,
    ) -> JidrResult<Value> {
        let id = self.next;
        self.next += 1;
        let mut req = json!({ "id": id, "method": method, "params": params });
        if let Some(t) = token {
            req["token"] = json!(t);
        }
        let mut line = req.to_string();
        line.push('\n');
        let io = |e: std::io::Error| JidrError::unexpected(format!("simulator runner: {e}"));
        self.writer.write_all(line.as_bytes()).await.map_err(io)?;
        let mut reply = String::new();
        loop {
            reply.clear();
            let n = tokio::time::timeout(timeout, self.reader.read_line(&mut reply))
                .await
                .map_err(|_| {
                    JidrError::unexpected(format!(
                        "the simulator runner didn't answer {method} within {} s",
                        timeout.as_secs()
                    ))
                })?
                .map_err(io)?;
            if n == 0 {
                return Err(JidrError::unexpected(
                    "the simulator runner closed the connection",
                ));
            }
            let v: Value = serde_json::from_str(reply.trim())
                .map_err(|e| JidrError::unexpected(format!("simulator runner reply: {e}")))?;
            if v["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                let msg = err["message"].as_str().unwrap_or("error");
                return Err(JidrError::usage(format!("{method}: {msg}")));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

impl Runner {
    /// Starts the runner on a booted simulator and waits for its `ping`.
    pub async fn start(xctestrun: &Path, udid: &str, log: &Path) -> JidrResult<Arc<Self>> {
        let port = free_port()?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        if let Some(dir) = log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let log_file = std::fs::File::create(log)
            .map_err(|e| JidrError::unexpected(format!("{}: {e}", log.display())))?;
        let err_file = log_file
            .try_clone()
            .map_err(|e| JidrError::unexpected(format!("{}: {e}", log.display())))?;
        let mut child = tokio::process::Command::new("/usr/bin/xcrun")
            .arg("xcodebuild")
            .arg("test-without-building")
            .arg("-xctestrun")
            .arg(xctestrun)
            .arg("-destination")
            .arg(format!("id={udid}"))
            .env("TEST_RUNNER_JIDR_PORT", port.to_string())
            .env("TEST_RUNNER_JIDR_TOKEN", &token)
            .stdin(Stdio::null())
            .stdout(log_file)
            .stderr(err_file)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| JidrError::sdk(format!("cannot run xcodebuild: {e}")))?;
        let started = Instant::now();
        let conn = loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(JidrError::sdk(format!(
                    "the simulator input runner exited ({status}) before it answered; see {}",
                    log.display()
                )));
            }
            if let Ok(c) = Conn::open(port, &token).await {
                break c;
            }
            if started.elapsed() > START_TIMEOUT {
                let _ = child.kill().await;
                return Err(JidrError::unexpected(format!(
                    "the simulator input runner didn't start within {} s; see {}",
                    START_TIMEOUT.as_secs(),
                    log.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        tracing::info!(%udid, port, ms = started.elapsed().as_millis() as u64, "simulator runner started");
        Ok(Arc::new(Self {
            child: Mutex::new(Some(child)),
            conn: Mutex::new(conn),
            port,
            last_used: std::sync::Mutex::new(Instant::now()),
            screen: std::sync::Mutex::new(None),
        }))
    }

    pub async fn call(&self, method: &str, params: Value) -> JidrResult<Value> {
        *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        let mut c = self.conn.lock().await;
        let v = c.request(method, params, None, CALL_TIMEOUT).await;
        if method == "rotate" {
            *self.screen.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        v
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }

    /// The screen size and scale, cached until a rotation.
    pub async fn screen(&self) -> JidrResult<Screen> {
        if let Some(s) = *self.screen.lock().unwrap_or_else(|e| e.into_inner()) {
            return Ok(s);
        }
        let v = self.call("screen", json!({})).await?;
        let s = Screen {
            width: v["width"].as_f64().unwrap_or(0.0),
            height: v["height"].as_f64().unwrap_or(0.0),
            scale: v["scale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0),
        };
        *self.screen.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
        Ok(s)
    }

    pub async fn alive(&self) -> bool {
        let mut child = self.child.lock().await;
        match child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Asks the runner to stop, then makes sure xcodebuild is gone.
    pub async fn stop(&self) {
        if let Ok(mut c) = tokio::time::timeout(Duration::from_secs(2), self.conn.lock()).await {
            let _ = c
                .request("stop", json!({}), None, Duration::from_secs(5))
                .await;
        }
        if let Some(mut child) = self.child.lock().await.take()
            && tokio::time::timeout(Duration::from_secs(15), child.wait())
                .await
                .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

fn free_port() -> JidrResult<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| JidrError::unexpected(format!("no free port: {e}")))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| JidrError::unexpected(format!("no free port: {e}")))
}

/// Runners by UDID, and the built `.xctestrun`.
#[derive(Debug, Default)]
pub struct Runners {
    by_udid: Mutex<HashMap<String, Arc<Runner>>>,
    /// One start at a time per simulator; other simulators aren't held up.
    starting: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    xctestrun: Mutex<Option<PathBuf>>,
}

impl Runners {
    async fn running(&self, udid: &str) -> Option<Arc<Runner>> {
        let r = self.by_udid.lock().await.get(udid).cloned()?;
        if r.alive().await {
            return Some(r);
        }
        self.by_udid.lock().await.remove(udid);
        None
    }

    /// The simulator's runner, built and started if needed.
    pub async fn get(
        &self,
        udid: &str,
        cache: &Path,
        logs: &Path,
        report: &(dyn Fn(&str) + Send + Sync),
    ) -> JidrResult<Arc<Runner>> {
        if let Some(r) = self.running(udid).await {
            return Ok(r);
        }
        let gate = self
            .starting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(udid.to_string())
            .or_default()
            .clone();
        let _starting = gate.lock().await;
        if let Some(r) = self.running(udid).await {
            return Ok(r);
        }
        let xctestrun = {
            let mut x = self.xctestrun.lock().await;
            match x.as_ref().filter(|p| p.is_file()) {
                Some(p) => p.clone(),
                None => {
                    let built = build(cache, report).await?;
                    *x = Some(built.clone());
                    built
                }
            }
        };
        report("Starting the simulator input runner");
        let log = logs.join("sim-runner").join(format!("{udid}.log"));
        let r = Runner::start(&xctestrun, udid, &log).await?;
        self.by_udid
            .lock()
            .await
            .insert(udid.to_string(), r.clone());
        Ok(r)
    }

    /// Stops a simulator's runner, if any.
    pub async fn stop(&self, udid: &str) {
        let r = self.by_udid.lock().await.remove(udid);
        if let Some(r) = r {
            r.stop().await;
        }
    }

    /// Stops runners idle past [`IDLE`] or on simulators that aren't
    /// booted any more.
    pub async fn sweep(&self, booted: &[String]) {
        let stale: Vec<(String, Arc<Runner>)> = {
            let map = self.by_udid.lock().await;
            map.iter()
                .filter(|(u, r)| r.idle_for() > IDLE || !booted.contains(u))
                .map(|(u, r)| (u.clone(), r.clone()))
                .collect()
        };
        for (udid, r) in stale {
            self.by_udid.lock().await.remove(&udid);
            r.stop().await;
        }
    }
}

/// A key name to what the runner does with it: a hardware button, or text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Button(&'static str),
    Text(&'static str),
}

/// Maps Jidr's key names (and the W3C ones MCP uses) for simulators.
pub fn key_action(key: &str) -> Option<KeyAction> {
    Some(match key.trim().to_ascii_lowercase().as_str() {
        "home" | "gohome" => KeyAction::Button("home"),
        "volume_up" | "audiovolumeup" => KeyAction::Button("volume_up"),
        "volume_down" | "audiovolumedown" => KeyAction::Button("volume_down"),
        "enter" | "return" => KeyAction::Text("\n"),
        "backspace" | "delete" | "del" => KeyAction::Text("\u{8}"),
        "tab" => KeyAction::Text("\t"),
        "escape" | "esc" => KeyAction::Text("\u{1b}"),
        _ => return None,
    })
}

/// Pixels (screenshot space) to points.
pub fn to_points(px: f64, scale: f64) -> f64 {
    px / scale.max(0.1)
}

/// The runner's tree with frames converted from points to pixels, so
/// they match screenshots and tap coordinates.
pub fn tree_in_pixels(mut tree: Value, scale: f64) -> Value {
    if let Some(elements) = tree.get_mut("elements").and_then(Value::as_array_mut) {
        for e in elements {
            if let Some(f) = e.get_mut("frame").and_then(Value::as_object_mut) {
                for k in ["x", "y", "width", "height"] {
                    if let Some(v) = f.get(k).and_then(Value::as_f64) {
                        f.insert(k.into(), json!((v * scale).round()));
                    }
                }
            }
        }
    }
    tree["units"] = json!("pixels");
    tree
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_buttons_or_text() {
        assert_eq!(key_action("GoHome"), Some(KeyAction::Button("home")));
        assert_eq!(
            key_action("volume_up"),
            Some(KeyAction::Button("volume_up"))
        );
        assert_eq!(key_action("Enter"), Some(KeyAction::Text("\n")));
        assert_eq!(key_action("Backspace"), Some(KeyAction::Text("\u{8}")));
        assert_eq!(key_action("GoBack"), None);
    }

    #[test]
    fn tree_frames_become_pixels() {
        let t = json!({ "bundle_id": "a", "elements": [
            { "type": "button", "frame": { "x": 10.0, "y": 20.5, "width": 100.0, "height": 44.0 } },
            { "type": "other", "frame": null }
        ]});
        let p = tree_in_pixels(t, 3.0);
        assert_eq!(p["elements"][0]["frame"]["x"], 30.0);
        assert_eq!(p["elements"][0]["frame"]["y"], 62.0);
        assert_eq!(p["elements"][1]["frame"], Value::Null);
        assert_eq!(p["units"], "pixels");
        assert_eq!(to_points(300.0, 3.0), 100.0);
    }

    #[test]
    fn source_hash_changes_with_content() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.swift"), "1").unwrap();
        let h1 = source_hash(d.path());
        std::fs::write(d.path().join("a.swift"), "2").unwrap();
        assert_ne!(h1, source_hash(d.path()));
    }
}
