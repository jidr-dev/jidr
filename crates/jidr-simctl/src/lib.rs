//! Async wrapper around `simctl` (Xcode's simulator tool) for the
//! `simulator.*` commands (D50). Every call runs the `simctl` binary that
//! `xcrun --find simctl` names, so signals (SIGINT to finish a recording)
//! reach it directly rather than an `xcrun` parent. Errors map to Jidr error
//! codes with what to do about them (install Xcode, select it, accept its
//! licence: Jidr never accepts Apple's licence for the user).
//!
//! Only public `simctl` subcommands are used (`simctl help`, Xcode 27.1).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use jidr_core::{ErrorCode, JidrError, JidrResult};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

pub mod parse;

pub use parse::{Runtime, SimApp, SimDevice};

/// Overrides the `simctl` binary (tests use a fake script).
pub const SIMCTL_ENV: &str = "JIDR_SIMCTL";
/// Default timeout for one `simctl` run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Booting can take minutes on a busy Mac (first boot of a new device).
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(600);

const XCODE_HINT: &str = "iOS simulators need full Xcode (from the Mac App Store or developer.apple.com), not only the Command Line Tools; then run `sudo xcode-select -s /Applications/Xcode.app` and open Xcode once";

#[derive(Debug, Clone)]
pub struct Simctl {
    bin: PathBuf,
    timeout: Duration,
}

/// Captured result of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
}

/// Status bar overrides (`simctl status_bar override`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusBar {
    pub time: Option<String>,
    pub data_network: Option<String>,
    pub wifi_bars: Option<u8>,
    pub cellular_bars: Option<u8>,
    pub operator_name: Option<String>,
    pub battery_state: Option<String>,
    pub battery_level: Option<u8>,
}

impl StatusBar {
    fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        let mut push = |flag: &str, v: Option<String>| {
            if let Some(v) = v {
                a.push(flag.to_string());
                a.push(v);
            }
        };
        push("--time", self.time.clone());
        push("--dataNetwork", self.data_network.clone());
        push("--wifiBars", self.wifi_bars.map(|b| b.to_string()));
        push("--cellularBars", self.cellular_bars.map(|b| b.to_string()));
        push("--operatorName", self.operator_name.clone());
        push("--batteryState", self.battery_state.clone());
        push("--batteryLevel", self.battery_level.map(|b| b.to_string()));
        a
    }

    pub fn is_empty(&self) -> bool {
        self.args().is_empty()
    }
}

impl Simctl {
    /// A `simctl` at `bin` (tests).
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Finds `simctl`: `$JIDR_SIMCTL`, else `xcrun --find simctl` on macOS.
    /// Elsewhere, or without Xcode, a typed `sdk` error says what to do.
    pub async fn detect() -> JidrResult<Self> {
        if let Some(p) = std::env::var_os(SIMCTL_ENV) {
            return Ok(Self::new(PathBuf::from(p)));
        }
        if !cfg!(target_os = "macos") {
            return Err(JidrError::sdk(
                "iOS simulators are not supported on this platform (they need macOS and Xcode)",
            ));
        }
        let out = Command::new("/usr/bin/xcrun")
            .args(["--find", "simctl"])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| JidrError::sdk(format!("cannot run xcrun: {e}; {XCODE_HINT}")))?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || path.is_empty() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(JidrError::sdk(format!("{err}; {XCODE_HINT}"))
                .with_data(serde_json::json!({ "xcode": "missing" })));
        }
        Ok(Self::new(path))
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    fn spawn_error(&self, e: std::io::Error) -> JidrError {
        JidrError::sdk(format!(
            "cannot run simctl at {}: {e}; {XCODE_HINT}",
            self.bin.display()
        ))
    }

    /// Starts `simctl` without waiting (`io recordVideo`, `spawn … log
    /// stream`); the child is killed when dropped.
    pub fn spawn(&self, args: &[&str]) -> JidrResult<Child> {
        self.command(args).spawn().map_err(|e| self.spawn_error(e))
    }

    /// Runs `simctl` and returns its output, whatever the exit status.
    pub async fn raw(&self, args: &[&str]) -> JidrResult<Output> {
        self.raw_with_stdin(args, None).await
    }

    async fn raw_with_stdin(&self, args: &[&str], stdin: Option<&[u8]>) -> JidrResult<Output> {
        let mut cmd = self.command(args);
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd.spawn().map_err(|e| self.spawn_error(e))?;
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(bytes)
                .await
                .map_err(|e| JidrError::unexpected(format!("simctl {}: {e}", args.join(" "))))?;
            drop(pipe);
        }
        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                JidrError::unexpected(format!(
                    "simctl {} timed out after {} s",
                    args.join(" "),
                    self.timeout.as_secs()
                ))
            })?
            .map_err(|e| JidrError::unexpected(format!("simctl {}: {e}", args.join(" "))))?;
        Ok(Output {
            status: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Runs `simctl` and fails unless it exits 0.
    pub async fn run(&self, args: &[&str]) -> JidrResult<Output> {
        let out = self.raw(args).await?;
        if out.success() {
            Ok(out)
        } else {
            Err(classify(args, &out))
        }
    }

    async fn run_with_stdin(&self, args: &[&str], stdin: &[u8]) -> JidrResult<Output> {
        let out = self.raw_with_stdin(args, Some(stdin)).await?;
        if out.success() {
            Ok(out)
        } else {
            Err(classify(args, &out))
        }
    }

    // ---- listing --------------------------------------------------------

    pub async fn devices(&self) -> JidrResult<Vec<SimDevice>> {
        let out = self.run(&["list", "devices", "-j"]).await?;
        parse::devices(&out.stdout)
    }

    pub async fn runtimes(&self) -> JidrResult<Vec<Runtime>> {
        let out = self.run(&["list", "runtimes", "-j"]).await?;
        parse::runtimes(&out.stdout)
    }

    pub async fn device_types(&self) -> JidrResult<Vec<jidr_core::SimDeviceTypeInfo>> {
        let out = self.run(&["list", "devicetypes", "-j"]).await?;
        parse::device_types(&out.stdout)
    }

    // ---- lifecycle ------------------------------------------------------

    /// `simctl create`: returns the new UDID.
    pub async fn create(&self, name: &str, device_type: &str, runtime: &str) -> JidrResult<String> {
        let out = self.run(&["create", name, device_type, runtime]).await?;
        udid_from(&out.stdout, "create")
    }

    /// `simctl clone` of a shut-down simulator: returns the new UDID.
    pub async fn clone_device(&self, udid: &str, name: &str) -> JidrResult<String> {
        let out = self.run(&["clone", udid, name]).await?;
        udid_from(&out.stdout, "clone")
    }

    pub async fn rename(&self, udid: &str, name: &str) -> JidrResult<()> {
        self.run(&["rename", udid, name]).await.map(drop)
    }

    pub async fn erase(&self, udid: &str) -> JidrResult<()> {
        self.run(&["erase", udid]).await.map(drop)
    }

    pub async fn delete(&self, udid: &str) -> JidrResult<()> {
        self.run(&["delete", udid]).await.map(drop)
    }

    /// Boots and waits until it has finished booting (`bootstatus`). An
    /// already booted simulator is fine.
    pub async fn boot(&self, udid: &str) -> JidrResult<()> {
        let slow = self.clone().with_timeout(BOOT_TIMEOUT);
        let out = slow.raw(&["boot", udid]).await?;
        if !out.success() && !in_state(&out, "Booted") {
            return Err(classify(&["boot", udid], &out));
        }
        slow.run(&["bootstatus", udid]).await.map(drop)
    }

    /// Shuts down; an already shut-down simulator is fine.
    pub async fn shutdown(&self, udid: &str) -> JidrResult<()> {
        let out = self.raw(&["shutdown", udid]).await?;
        if out.success() || in_state(&out, "Shutdown") {
            Ok(())
        } else {
            Err(classify(&["shutdown", udid], &out))
        }
    }

    // ---- screen ---------------------------------------------------------

    /// The screen as a PNG. Written to a temp file: on Xcode 27.1 `-` makes
    /// a file named `-` instead of writing to stdout.
    pub async fn screenshot_png(&self, udid: &str) -> JidrResult<Vec<u8>> {
        let dir = temp_dir()?;
        let path = dir.path().join("screen.png");
        let p = path_str(&path)?;
        self.run(&["io", udid, "screenshot", "--type=png", p])
            .await?;
        let png = std::fs::read(&path)
            .map_err(|e| JidrError::unexpected(format!("reading the screenshot: {e}")))?;
        if !png.starts_with(b"\x89PNG") {
            return Err(JidrError::unexpected("simctl gave no PNG"));
        }
        Ok(png)
    }

    /// Starts recording the screen to `path` (QuickTime, H.264); send SIGINT
    /// with [`interrupt`] to finish the file.
    pub fn record(&self, udid: &str, path: &Path) -> JidrResult<Child> {
        let p = path_str(path)?;
        self.spawn(&["io", udid, "recordVideo", "--codec=h264", "--force", p])
    }

    // ---- apps -----------------------------------------------------------

    /// Installs a simulator `.app` (not an App Store `.ipa`).
    pub async fn install(&self, udid: &str, app: &Path) -> JidrResult<()> {
        let p = path_str(app)?;
        self.clone()
            .with_timeout(Duration::from_secs(600))
            .run(&["install", udid, p])
            .await
            .map(drop)
    }

    pub async fn uninstall(&self, udid: &str, bundle: &str) -> JidrResult<()> {
        self.run(&["uninstall", udid, bundle]).await.map(drop)
    }

    /// Installed apps (`listapps` prints an old-style plist; `plutil`
    /// converts it to JSON).
    pub async fn apps(&self, udid: &str) -> JidrResult<Vec<SimApp>> {
        let out = self.run(&["listapps", udid]).await?;
        let text = out.stdout.trim();
        if text.starts_with('{') && serde_json::from_str::<serde_json::Value>(text).is_ok() {
            return parse::apps(text);
        }
        let dir = temp_dir()?;
        let path = dir.path().join("apps.plist");
        std::fs::write(&path, text)
            .map_err(|e| JidrError::unexpected(format!("writing the app list: {e}")))?;
        let json = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&path)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| JidrError::unexpected(format!("plutil: {e}")))?;
        if !json.status.success() {
            return Err(JidrError::unexpected(format!(
                "plutil couldn't read simctl's app list: {}",
                String::from_utf8_lossy(&json.stderr).trim()
            )));
        }
        parse::apps(&String::from_utf8_lossy(&json.stdout))
    }

    /// Launches an app and returns its pid.
    pub async fn launch(
        &self,
        udid: &str,
        bundle: &str,
        args: &[String],
        relaunch: bool,
    ) -> JidrResult<Option<u32>> {
        let mut a = vec!["launch"];
        if relaunch {
            a.push("--terminate-running-process");
        }
        a.extend([udid, bundle]);
        a.extend(args.iter().map(String::as_str));
        let out = self.run(&a).await?;
        Ok(out
            .stdout
            .rsplit(':')
            .next()
            .and_then(|pid| pid.trim().parse().ok()))
    }

    pub async fn terminate(&self, udid: &str, bundle: &str) -> JidrResult<()> {
        self.run(&["terminate", udid, bundle]).await.map(drop)
    }

    /// An installed app's folder on this Mac: `app`, `data` or `groups`.
    pub async fn app_container(&self, udid: &str, bundle: &str, kind: &str) -> JidrResult<PathBuf> {
        let out = self.run(&["get_app_container", udid, bundle, kind]).await?;
        let p = out.stdout.trim();
        if p.is_empty() {
            return Err(JidrError::not_found(format!(
                "{bundle} has no {kind} container"
            )));
        }
        Ok(PathBuf::from(p))
    }

    // ---- environment ----------------------------------------------------

    /// Opens a URL. `Ok(false)` when simctl timed out waiting for the
    /// simulator to confirm: on a busy Mac the URL still opens (seen with
    /// Safari on iOS 26.1), so that's not a failure.
    pub async fn open_url(&self, udid: &str, url: &str) -> JidrResult<bool> {
        let args = ["openurl", udid, url];
        let out = self.raw(&args).await?;
        if out.success() {
            Ok(true)
        } else if timed_out(&out) {
            Ok(false)
        } else {
            Err(classify(&args, &out))
        }
    }

    pub async fn location_set(&self, udid: &str, lat: f64, lon: f64) -> JidrResult<()> {
        let at = format!("{lat},{lon}");
        self.run(&["location", udid, "set", &at]).await.map(drop)
    }

    /// Moves along `points` (lat, lon) at `speed_mps`, with an update every
    /// `interval_s` seconds. Waypoints go through stdin, so long routes fit.
    pub async fn location_route(
        &self,
        udid: &str,
        points: &[(f64, f64)],
        speed_mps: f64,
        interval_s: f64,
    ) -> JidrResult<()> {
        if points.len() < 2 {
            return Err(JidrError::usage("a route needs at least two points"));
        }
        let speed = format!("--speed={speed_mps}");
        let interval = format!("--interval={interval_s}");
        let input: String = points
            .iter()
            .map(|(lat, lon)| format!("{lat},{lon}\n"))
            .collect();
        self.run_with_stdin(
            &["location", udid, "start", &speed, &interval, "-"],
            input.as_bytes(),
        )
        .await
        .map(drop)
    }

    pub async fn location_clear(&self, udid: &str) -> JidrResult<()> {
        self.run(&["location", udid, "clear"]).await.map(drop)
    }

    /// `ui <option> [value]`: prints the current value without one.
    pub async fn ui(&self, udid: &str, option: &str, value: Option<&str>) -> JidrResult<String> {
        let mut a = vec!["ui", udid, option];
        a.extend(value);
        let out = self.run(&a).await?;
        Ok(out.stdout.trim().to_string())
    }

    pub async fn status_bar(&self, udid: &str, bar: &StatusBar) -> JidrResult<()> {
        let args = bar.args();
        let mut a = vec!["status_bar", udid, "override"];
        a.extend(args.iter().map(String::as_str));
        self.run(&a).await.map(drop)
    }

    pub async fn status_bar_clear(&self, udid: &str) -> JidrResult<()> {
        self.run(&["status_bar", udid, "clear"]).await.map(drop)
    }

    /// `privacy <grant|revoke|reset> <service> [bundle]`.
    pub async fn privacy(
        &self,
        udid: &str,
        action: &str,
        service: &str,
        bundle: Option<&str>,
    ) -> JidrResult<()> {
        let mut a = vec!["privacy", udid, action, service];
        a.extend(bundle);
        self.run(&a).await.map(drop)
    }

    /// Sends a push notification payload (JSON with an `aps` key).
    pub async fn push(&self, udid: &str, bundle: &str, payload: &str) -> JidrResult<()> {
        self.run_with_stdin(&["push", udid, bundle, "-"], payload.as_bytes())
            .await
            .map(drop)
    }

    pub async fn pbcopy(&self, udid: &str, text: &str) -> JidrResult<()> {
        self.run_with_stdin(&["pbcopy", udid], text.as_bytes())
            .await
            .map(drop)
    }

    pub async fn pbpaste(&self, udid: &str) -> JidrResult<String> {
        Ok(self.run(&["pbpaste", udid]).await?.stdout)
    }

    /// Adds photos, videos or contacts (vCard) to the simulator's libraries.
    pub async fn add_media(&self, udid: &str, files: &[PathBuf]) -> JidrResult<()> {
        let mut a = vec!["addmedia".to_string(), udid.to_string()];
        for f in files {
            a.push(path_str(f)?.to_string());
        }
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        self.clone()
            .with_timeout(Duration::from_secs(300))
            .run(&a)
            .await
            .map(drop)
    }

    /// Trusts a CA certificate (PEM or DER), e.g. a debugging proxy's.
    pub async fn trust_cert(&self, udid: &str, cert: &Path) -> JidrResult<()> {
        let p = path_str(cert)?;
        self.run(&["keychain", udid, "add-root-cert", p])
            .await
            .map(drop)
    }

    /// Follows the simulator's unified log as NDJSON lines.
    pub fn log_stream(&self, udid: &str, predicate: Option<&str>) -> JidrResult<Child> {
        let mut a = vec!["spawn", udid, "log", "stream", "--style", "ndjson"];
        if let Some(p) = predicate {
            a.extend(["--predicate", p]);
        }
        self.spawn(&a)
    }
}

/// Sends SIGINT to a child (`recordVideo` finalises its file on it).
pub async fn interrupt(child: &Child) -> JidrResult<()> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    let status = Command::new("/bin/kill")
        .args(["-INT", &pid.to_string()])
        .stdin(Stdio::null())
        .status()
        .await
        .map_err(|e| JidrError::unexpected(format!("kill -INT {pid}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(JidrError::unexpected(format!("kill -INT {pid} failed")))
    }
}

/// simctl gave up waiting for the simulator (`NSPOSIXErrorDomain` code 60,
/// "Operation timed out").
fn timed_out(out: &Output) -> bool {
    let text = format!("{}{}", out.stderr, out.stdout);
    text.contains("Operation timed out") || text.contains("NSPOSIXErrorDomain, code=60")
}

/// `Unable to … in current state: Booted`.
fn in_state(out: &Output, state: &str) -> bool {
    let text = format!("{}{}", out.stderr, out.stdout);
    text.contains("current state") && text.trim_end().ends_with(state)
}

/// Maps `simctl`'s error text to a Jidr error with a hint.
pub fn classify(args: &[&str], out: &Output) -> JidrError {
    let text = format!("{}{}", out.stderr, out.stdout);
    let msg = text.trim();
    let (code, hint) = if msg.contains("Invalid device") || msg.contains("No devices are booted") {
        (ErrorCode::NotFound, "")
    } else if msg.contains("unable to find utility")
        || msg.contains("invalid active developer path")
    {
        (ErrorCode::Sdk, XCODE_HINT)
    } else if msg.contains("license") {
        (
            ErrorCode::Sdk,
            "accept Xcode's licence yourself first: open Xcode, or run `sudo xcodebuild -license`",
        )
    } else if msg.contains("Invalid runtime") || msg.contains("Invalid device type") {
        (
            ErrorCode::NotFound,
            "`jidr sim runtimes` lists what's installed; download one with `jidr sim runtime-download ios`",
        )
    } else if msg.contains("current state") {
        (ErrorCode::Busy, "")
    } else {
        (ErrorCode::Sdk, "")
    };
    let mut message = format!("simctl {} failed: {msg}", args.join(" "));
    if !hint.is_empty() {
        message.push_str("; ");
        message.push_str(hint);
    }
    JidrError::new(code, message).with_data(serde_json::json!({ "status": out.status }))
}

fn udid_from(stdout: &str, what: &str) -> JidrResult<String> {
    let udid = stdout.trim();
    if is_udid(udid) {
        Ok(udid.to_string())
    } else {
        Err(JidrError::unexpected(format!(
            "simctl {what} returned no UDID: {udid:?}"
        )))
    }
}

/// `5CB58484-F3D8-4482-A8CA-6A979D33FB66`. Keeps simctl aliases (`booted`,
/// `all`) out of commands that must name one simulator.
pub fn is_udid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn temp_dir() -> JidrResult<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("jidr-simctl")
        .tempdir()
        .map_err(|e| JidrError::unexpected(format!("temp dir: {e}")))
}

fn path_str(p: &Path) -> JidrResult<&str> {
    p.to_str()
        .ok_or_else(|| JidrError::usage(format!("path isn't UTF-8: {}", p.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(stderr: &str) -> Output {
        Output {
            status: Some(1),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn udids() {
        assert!(is_udid("5CB58484-F3D8-4482-A8CA-6A979D33FB66"));
        assert!(!is_udid("booted"));
        assert!(!is_udid("all"));
        assert!(!is_udid("5CB58484-F3D8-4482-A8CA-6A979D33FB6"));
    }

    #[test]
    fn errors_classify() {
        let e = classify(&["boot", "x"], &out("Invalid device: x"));
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = classify(
            &["list"],
            &out("xcrun: error: unable to find utility \"simctl\""),
        );
        assert_eq!(e.code, ErrorCode::Sdk);
        assert!(e.message.contains("xcode-select"), "{e}");
        let e = classify(
            &["list"],
            &out("You have not agreed to the Xcode license agreements."),
        );
        assert!(e.message.contains("xcodebuild -license"), "{e}");
    }

    #[test]
    fn state_errors_are_recognised() {
        assert!(in_state(
            &out("Unable to boot device in current state: Booted\n"),
            "Booted"
        ));
        assert!(!in_state(&out("Invalid device"), "Booted"));
    }

    // Regression: `openurl` timed out under load although Safari opened the
    // URL, and Jidr reported a failure.
    #[test]
    fn openurl_timeouts_are_recognised() {
        let o = out(
            "An error was encountered processing the command (domain=NSPOSIXErrorDomain, code=60):\nSimulator device failed to open https://jidr.ly.\nOperation timed out\n",
        );
        assert!(timed_out(&o));
        assert!(!timed_out(&out("Invalid device: x")));
    }

    #[test]
    fn status_bar_flags() {
        let b = StatusBar {
            time: Some("9:41".into()),
            battery_level: Some(100),
            ..Default::default()
        };
        assert_eq!(
            b.args(),
            ["--time", "9:41", "--batteryLevel", "100"].map(String::from)
        );
        assert!(StatusBar::default().is_empty());
    }
}
