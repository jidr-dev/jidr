//! iOS simulators on macOS over `simctl` (D50): `simulator.*`.
//!
//! A simulator isn't a Jidr device: CoreSimulator owns it, in Xcode's own
//! device set, so Xcode, Simulator.app and other tools see the same ones.
//! Commands name a simulator by UDID (or by its name when that's unique).
//! Jidr remembers which simulators it created (`simulators.json`) and only
//! erases or deletes those unless the caller passes `force`.
//!
//! `simulator.lease` gives each agent session its own fresh simulator: it is
//! created, booted, and tied to the caller's connection (an agent's `jidr
//! mcp`) or a TTL like pool leases (D42). When the lease ends the reaper
//! shuts it down and deletes it.
//!
//! A poller of `simctl list devices -j`, started by the first call,
//! publishes `simulator.added`, `simulator.changed` and `simulator.removed`.
//!
//! Tap, swipe, text, keys, rotation and the UI tree go through Jidr's XCTest
//! runner (`sim_runner`), built on this Mac on first use. Coordinates are
//! screenshot pixels, like the rest of Jidr; the runner works in points.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jidr_core::provider::{
    CallCtx, CommandProvider, CommandSpec, DaemonContext, NoParams, parse_params,
};
use jidr_core::{
    JidrError, JidrResult, SimConnectInfo, SimDeviceTypeInfo, SimHolder, SimLease, SimRuntimeInfo,
    SimState, Simulator,
};
use jidr_simctl::{SimDevice, Simctl, StatusBar};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::sim_runner::{KeyAction, to_points};
use crate::state::State;

/// What a booted simulator is expected to cost (simslim measured ~4 GB
/// `phys_footprint` for a stock iOS 26.5 simulator on an M1 Pro).
pub const SIM_EXPECTED_MB: u64 = 4096;
/// How often the poller lists simulators.
const POLL_EVERY: Duration = Duration::from_secs(3);
/// A TTL lease's default lifetime (the CLI's).
pub const DEFAULT_TTL: Duration = Duration::from_secs(2 * 60 * 60);

// ---- params -------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SimParams {
    /// The simulator's UDID (`simulator.list`), or its name when only one
    /// simulator has it.
    pub device: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateParams {
    pub name: String,
    /// Device type id or name, e.g. `iPhone 17 Pro` (default: the newest iPhone the runtime supports).
    #[serde(default)]
    pub device_type: Option<String>,
    /// Runtime id, name or version, e.g. `iOS 26.1` or `26.1` (default: the newest iOS runtime with a regular iPhone, or with `device_type`).
    #[serde(default)]
    pub runtime: Option<String>,
    /// Boot it once created.
    #[serde(default)]
    pub boot: bool,
    /// Agents may erase and delete it without `--allow-destructive` (D5).
    /// MCP sets this for simulators an agent creates.
    #[serde(default)]
    pub agent_owned: bool,
    /// Start even past the host RAM budget.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseParams {
    /// Device type id or name (default: the newest iPhone).
    #[serde(default)]
    pub device_type: Option<String>,
    /// Runtime id, name or version (default: the newest iOS runtime with a regular iPhone, or with `device_type`).
    #[serde(default)]
    pub runtime: Option<String>,
    /// The simulator's name (default `Jidr agent <id>`).
    #[serde(default)]
    pub name: Option<String>,
    /// Lease lifetime in seconds. Without it the lease lasts as long as this
    /// client connection, plus 2 minutes.
    #[serde(default)]
    pub ttl_s: Option<u64>,
    /// Start even past the host RAM budget.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseParams {
    /// The lease id from `simulator.lease`.
    #[serde(default)]
    pub lease: Option<Uuid>,
    /// Or the leased simulator's UDID or name.
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloneParams {
    pub device: String,
    /// The copy's name.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameParams {
    pub device: String,
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BootParams {
    pub device: String,
    /// Start even past the host RAM budget.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForceParams {
    pub device: String,
    /// Required for a simulator Jidr didn't create.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotParams {
    pub device: String,
    /// Where to save the PNG (default `~/Pictures/Jidr/<name> <time>.png`).
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordParams {
    pub device: String,
    /// Where to save the QuickTime movie (default `~/Movies/Jidr/<name> <time>.mov`).
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Stop on its own after this many seconds (default 600).
    #[serde(default)]
    pub max_seconds: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallParams {
    pub device: String,
    /// A simulator build: an `.app` folder, or a `.zip` holding one.
    pub app: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BundleParams {
    pub device: String,
    /// The app's bundle id, e.g. `com.example.app`.
    pub bundle: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchParams {
    pub device: String,
    pub bundle: String,
    /// Arguments passed to the app.
    #[serde(default)]
    pub args: Vec<String>,
    /// Quit it first if it's running.
    #[serde(default)]
    pub relaunch: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PushFileParams {
    pub device: String,
    /// The app whose data container receives the file.
    pub bundle: String,
    /// A file on this Mac.
    pub local: PathBuf,
    /// Path inside the app's data container (default `Documents/`).
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullFileParams {
    pub device: String,
    pub bundle: String,
    /// Path inside the app's data container, e.g. `Documents/log.txt`.
    pub remote: String,
    /// Where to save it on this Mac.
    pub local: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogParams {
    pub device: String,
    /// How far back, in seconds (default 60).
    #[serde(default)]
    pub seconds: Option<u32>,
    /// Only the last N lines.
    #[serde(default)]
    pub lines: Option<u32>,
    /// A `log` predicate, e.g. `process == "MyApp"`.
    #[serde(default)]
    pub predicate: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UrlParams {
    pub device: String,
    /// A URL or deep link, e.g. `myapp://settings`.
    pub url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocationParams {
    pub device: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteParams {
    pub device: String,
    /// A GPX or KML file.
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// Or the points as `[lat, lon]` pairs.
    #[serde(default)]
    pub points: Vec<[f64; 2]>,
    /// Speed in metres per second (default 20).
    #[serde(default)]
    pub speed_mps: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppearanceParams {
    pub device: String,
    /// `light` or `dark`; without it the current one is returned.
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContentSizeParams {
    pub device: String,
    /// `extra-small` … `extra-extra-extra-large`, `accessibility-medium` …,
    /// `increment` or `decrement`; without it the current one is returned.
    #[serde(default)]
    pub size: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusBarParams {
    pub device: String,
    /// Remove every override.
    #[serde(default)]
    pub clear: bool,
    /// e.g. `9:41`, or an ISO date.
    #[serde(default)]
    pub time: Option<String>,
    /// `hide`, `wifi`, `3g`, `4g`, `lte`, `lte-a`, `lte+`, `5g`, `5g+`, `5g-uwb`, `5g-uc`.
    #[serde(default)]
    pub data_network: Option<String>,
    /// 0–3.
    #[serde(default)]
    pub wifi_bars: Option<u8>,
    /// 0–4.
    #[serde(default)]
    pub cellular_bars: Option<u8>,
    #[serde(default)]
    pub operator_name: Option<String>,
    /// `charging`, `charged` or `discharging`.
    #[serde(default)]
    pub battery_state: Option<String>,
    /// 0–100.
    #[serde(default)]
    pub battery_level: Option<u8>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrivacyParams {
    pub device: String,
    /// `grant`, `revoke` or `reset`.
    pub action: String,
    /// `all`, `calendar`, `contacts`, `contacts-limited`, `location`,
    /// `location-always`, `photos`, `photos-add`, `media-library`,
    /// `microphone`, `motion`, `reminders` or `siri`.
    pub service: String,
    /// The app (required for grant and revoke).
    #[serde(default)]
    pub bundle: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PushNotificationParams {
    pub device: String,
    pub bundle: String,
    /// The payload, with an `aps` object, e.g. `{"aps": {"alert": "Hi"}}`.
    pub payload: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PasteboardParams {
    pub device: String,
    /// Text to put on the pasteboard; without it the current text is returned.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MediaParams {
    pub device: String,
    /// Photos, videos or vCard contacts.
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CertParams {
    pub device: String,
    /// A CA certificate (PEM or DER), e.g. a debugging proxy's.
    pub cert: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TapParams {
    pub device: String,
    /// Screenshot pixels from the left.
    pub x: f64,
    /// Screenshot pixels from the top.
    pub y: f64,
    /// Hold this long (a long press).
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwipeParams {
    pub device: String,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    /// How long the swipe takes (default 300 ms).
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextParams {
    pub device: String,
    /// Text to type into the focused field (any characters).
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyParams {
    pub device: String,
    /// `home`, `volume_up`, `volume_down`, `enter`, `backspace`, `tab` (or
    /// `GoHome`, `Enter`, `Backspace`…).
    pub key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RotateParams {
    pub device: String,
    /// `portrait`, `landscape_left`, `landscape_right` or `portrait_upside_down`.
    pub orientation: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DownloadParams {
    /// `iOS`, `tvOS`, `watchOS` or `visionOS` (default `iOS`).
    #[serde(default)]
    pub platform: Option<String>,
    /// A specific OS version, e.g. `26.1` (default: the one this Xcode wants).
    #[serde(default)]
    pub version: Option<String>,
}

// ---- state --------------------------------------------------------------

/// A simulator Jidr created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owned {
    pub udid: String,
    pub agent: bool,
    pub created_at: DateTime<Utc>,
}

/// What `simulators.json` holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct OwnedFile {
    #[serde(default)]
    simulators: Vec<Owned>,
    #[serde(default)]
    leases: Vec<SimLease>,
}

/// Simulator state the daemon keeps: what Jidr created, leases, the last
/// listing, recordings.
#[derive(Debug)]
pub struct SimHub {
    file: PathBuf,
    simctl: tokio::sync::Mutex<Option<Simctl>>,
    owned: Mutex<OwnedFile>,
    /// The last listing (for the RAM budget and events).
    listing: Mutex<Vec<SimDevice>>,
    recordings: Mutex<HashMap<String, Arc<Notify>>>,
    watching: Mutex<bool>,
    cache: PathBuf,
    logs: PathBuf,
    /// XCTest input runners by UDID.
    pub(crate) runners: crate::sim_runner::Runners,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn grace() -> chrono::Duration {
    chrono::Duration::from_std(crate::leases::GRACE).unwrap_or_default()
}

impl SimHub {
    /// Loads `<data>/simulators.json`. Leases from before a daemon restart
    /// have no connection any more: they get the grace period, then the
    /// reaper takes them.
    pub fn load(file: PathBuf, cache: PathBuf, logs: PathBuf) -> Self {
        let mut owned: OwnedFile = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let deadline = Utc::now() + grace();
        for l in &mut owned.leases {
            if let SimHolder::Connection { .. } = l.holder {
                l.holder = SimHolder::Ttl;
                l.expires_at = Some(l.expires_at.map_or(deadline, |e| e.min(deadline)));
            }
        }
        Self {
            file,
            simctl: tokio::sync::Mutex::new(None),
            owned: Mutex::new(owned),
            listing: Mutex::new(Vec::new()),
            recordings: Mutex::new(HashMap::new()),
            watching: Mutex::new(false),
            cache,
            logs,
            runners: crate::sim_runner::Runners::default(),
        }
    }

    /// `simctl`, found once (and looked for again after a failure, so
    /// installing Xcode doesn't need a daemon restart).
    pub async fn simctl(&self) -> JidrResult<Simctl> {
        let mut s = self.simctl.lock().await;
        if let Some(s) = s.as_ref() {
            return Ok(s.clone());
        }
        let found = Simctl::detect().await?;
        *s = Some(found.clone());
        Ok(found)
    }

    fn save(&self, f: &OwnedFile) {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.file.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(f).unwrap_or_default())?;
            std::fs::rename(&tmp, &self.file)
        };
        if let Err(e) = write() {
            tracing::warn!(file = %self.file.display(), "saving simulators: {e}");
        }
    }

    fn update(&self, f: impl FnOnce(&mut OwnedFile)) {
        let mut o = lock(&self.owned);
        f(&mut o);
        self.save(&o);
    }

    pub fn owned(&self, udid: &str) -> Option<Owned> {
        lock(&self.owned)
            .simulators
            .iter()
            .find(|o| o.udid == udid)
            .cloned()
    }

    fn remember(&self, udid: &str, agent: bool) {
        self.update(|f| {
            f.simulators.retain(|o| o.udid != udid);
            f.simulators.push(Owned {
                udid: udid.to_string(),
                agent,
                created_at: Utc::now(),
            });
        });
    }

    fn forget(&self, udid: &str) {
        self.update(|f| {
            f.simulators.retain(|o| o.udid != udid);
            f.leases.retain(|l| l.udid != udid);
        });
    }

    pub fn leases(&self) -> Vec<SimLease> {
        lock(&self.owned).leases.clone()
    }

    pub fn lease_count(&self) -> usize {
        lock(&self.owned).leases.len()
    }

    pub fn lease_of(&self, udid: &str) -> Option<SimLease> {
        lock(&self.owned)
            .leases
            .iter()
            .find(|l| l.udid == udid)
            .cloned()
    }

    fn add_lease(&self, lease: SimLease) {
        self.update(|f| f.leases.push(lease));
    }

    /// Connection `conn` closed: its leases expire after the grace period.
    pub fn disconnected(&self, conn: u64, now: DateTime<Utc>) {
        let mut changed = false;
        let mut o = lock(&self.owned);
        for l in &mut o.leases {
            if l.holder == (SimHolder::Connection { id: conn }) && l.expires_at.is_none() {
                l.expires_at = Some(now + grace());
                changed = true;
            }
        }
        if changed {
            self.save(&o);
        }
    }

    /// Leases due for release as of `now`.
    pub fn expired(&self, now: DateTime<Utc>) -> Vec<SimLease> {
        lock(&self.owned)
            .leases
            .iter()
            .filter(|l| l.expires_at.is_some_and(|e| e <= now))
            .cloned()
            .collect()
    }

    /// Booted simulators from the last listing, for the RAM budget.
    pub fn booted(&self) -> Vec<(String, String)> {
        lock(&self.listing)
            .iter()
            .filter(|d| SimState::from_simctl(&d.state, d.available).is_booted())
            .map(|d| (d.udid.clone(), d.name.clone()))
            .collect()
    }

    pub fn booted_udids(&self) -> Vec<String> {
        self.booted().into_iter().map(|(u, _)| u).collect()
    }

    fn set_listing(&self, devices: &[SimDevice]) {
        *lock(&self.listing) = devices.to_vec();
    }
}

// ---- the provider -------------------------------------------------------

pub struct SimulatorProvider {
    state: Arc<State>,
}

impl std::fmt::Debug for SimulatorProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulatorProvider").finish_non_exhaustive()
    }
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new::<NoParams>(
            "simulator.list",
            "iOS, tvOS, watchOS and visionOS simulators on this Mac (Xcode's device set): state, runtime, whether Jidr created or leased them",
        ),
        CommandSpec::new::<SimParams>("simulator.details", "One simulator's details"),
        CommandSpec::new::<SimParams>(
            "simulator.connect_info",
            "The UDID and platform of a booted simulator",
        ),
        CommandSpec::new::<NoParams>("simulator.runtimes", "Installed simulator runtimes"),
        CommandSpec::new::<NoParams>(
            "simulator.device_types",
            "Simulator device types (iPhone 17 Pro, iPad Air…)",
        ),
        CommandSpec::new::<DownloadParams>(
            "simulator.runtime_download",
            "Download a simulator runtime with Xcode (`xcodebuild -downloadPlatform`, about 8 GB)",
        )
        .long_running(),
        CommandSpec::new::<CreateParams>("simulator.create", "Create a simulator").long_running(),
        CommandSpec::new::<LeaseParams>(
            "simulator.lease",
            "Get your own fresh, booted simulator for this session; it's shut down and deleted when the lease ends (this connection closes, or ttl_s)",
        )
        .long_running(),
        CommandSpec::new::<ReleaseParams>(
            "simulator.release",
            "End a simulator lease: the simulator is shut down and deleted",
        ),
        CommandSpec::new::<NoParams>("simulator.leases", "Simulator leases"),
        CommandSpec::new::<CloneParams>(
            "simulator.clone",
            "Copy a shut-down simulator (apps, data and settings)",
        ),
        CommandSpec::new::<RenameParams>("simulator.rename", "Rename a simulator"),
        CommandSpec::new::<BootParams>("simulator.boot", "Boot a simulator and wait until it's ready")
            .long_running(),
        CommandSpec::new::<SimParams>("simulator.shutdown", "Shut a simulator down"),
        CommandSpec::new::<ForceParams>(
            "simulator.erase",
            "Erase a simulator's apps, data and settings (force for one Jidr didn't create)",
        )
        .destructive(),
        CommandSpec::new::<ForceParams>(
            "simulator.delete",
            "Delete a simulator (force for one Jidr didn't create)",
        )
        .destructive(),
        CommandSpec::new::<ScreenshotParams>(
            "simulator.screenshot",
            "Save a PNG of a simulator's screen",
        ),
        CommandSpec::new::<RecordParams>(
            "simulator.record",
            "Record a simulator's screen (QuickTime, H.264) until simulator.record_stop or max_seconds",
        )
        .long_running(),
        CommandSpec::new::<SimParams>(
            "simulator.record_stop",
            "Stop a simulator's screen recording and save it",
        ),
        CommandSpec::new::<InstallParams>(
            "simulator.install",
            "Install a simulator build (.app, or a .zip holding one)",
        ),
        CommandSpec::new::<BundleParams>("simulator.uninstall", "Uninstall an app"),
        CommandSpec::new::<SimParams>("simulator.apps", "Installed apps"),
        CommandSpec::new::<LaunchParams>("simulator.launch", "Launch an app"),
        CommandSpec::new::<BundleParams>("simulator.terminate", "Quit an app"),
        CommandSpec::new::<PushFileParams>(
            "simulator.push_file",
            "Copy a file into an app's data container (default Documents/)",
        ),
        CommandSpec::new::<PullFileParams>(
            "simulator.pull_file",
            "Copy a file out of an app's data container",
        ),
        CommandSpec::new::<LogParams>("simulator.log", "Recent lines of a simulator's log"),
        CommandSpec::new::<UrlParams>("simulator.open_url", "Open a URL or deep link"),
        CommandSpec::new::<LocationParams>("simulator.location_set", "Set the simulated location"),
        CommandSpec::new::<RouteParams>(
            "simulator.location_route",
            "Move along a route (GPX/KML file or points) at a speed",
        ),
        CommandSpec::new::<SimParams>(
            "simulator.location_clear",
            "Stop a route and clear the simulated location",
        ),
        CommandSpec::new::<AppearanceParams>(
            "simulator.appearance",
            "Get or set light or dark mode",
        ),
        CommandSpec::new::<ContentSizeParams>(
            "simulator.content_size",
            "Get or set the text size (Dynamic Type)",
        ),
        CommandSpec::new::<StatusBarParams>(
            "simulator.status_bar",
            "Override what the status bar shows (time, signal, battery), or clear it",
        ),
        CommandSpec::new::<PrivacyParams>(
            "simulator.privacy",
            "Grant, revoke or reset an app's permission without a prompt",
        ),
        CommandSpec::new::<PushNotificationParams>(
            "simulator.push_notification",
            "Send an app a push notification",
        ),
        CommandSpec::new::<PasteboardParams>(
            "simulator.pasteboard",
            "Get or set the pasteboard text",
        ),
        CommandSpec::new::<MediaParams>(
            "simulator.add_media",
            "Add photos, videos or contacts",
        ),
        CommandSpec::new::<CertParams>(
            "simulator.trust_cert",
            "Trust a CA certificate (for a debugging proxy)",
        ),
        CommandSpec::new::<SimParams>(
            "simulator.input_prepare",
            "Start the XCTest input runner on a booted simulator (built once per Xcode version; tap and friends start it on demand)",
        )
        .long_running(),
        CommandSpec::new::<TapParams>(
            "simulator.tap",
            "Tap a simulator's screen (screenshot pixels); duration_ms for a long press",
        ),
        CommandSpec::new::<SwipeParams>(
            "simulator.swipe",
            "Swipe on a simulator's screen (screenshot pixels)",
        ),
        CommandSpec::new::<TextParams>(
            "simulator.text",
            "Type text into the focused field of the app in front",
        ),
        CommandSpec::new::<KeyParams>(
            "simulator.key",
            "Press home, volume or a text key (enter, backspace, tab)",
        ),
        CommandSpec::new::<RotateParams>("simulator.rotate", "Rotate a simulator"),
        CommandSpec::new::<SimParams>(
            "simulator.ui_tree",
            "The UI element tree of the app in front (type, identifier, label, value, frame in screenshot pixels)",
        ),
    ]
}

impl SimulatorProvider {
    pub fn new(state: Arc<State>) -> Self {
        Self { state }
    }

    fn hub(&self) -> &SimHub {
        &self.state.sims
    }
}

#[async_trait]
impl CommandProvider for SimulatorProvider {
    fn name(&self) -> &str {
        "simulator"
    }

    fn commands(&self) -> Vec<CommandSpec> {
        specs()
    }

    async fn call(&self, ctx: &CallCtx, method: &str, params: Value) -> JidrResult<Value> {
        let hub = self.hub();
        let simctl = hub.simctl().await?;
        self.watch(&simctl);
        match method {
            "simulator.list" => {
                parse_params::<NoParams>(params)?;
                to_value(self.list(&simctl).await?)
            }
            "simulator.details" => {
                let p: SimParams = parse_params(params)?;
                to_value(self.pick(&simctl, &p.device).await?)
            }
            "simulator.connect_info" => {
                let p: SimParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                to_value(SimConnectInfo {
                    udid: s.udid,
                    name: s.name,
                    platform: s.platform,
                })
            }
            "simulator.runtimes" => {
                parse_params::<NoParams>(params)?;
                let r: Vec<SimRuntimeInfo> = simctl
                    .runtimes()
                    .await?
                    .into_iter()
                    .map(|r| r.info)
                    .collect();
                to_value(r)
            }
            "simulator.device_types" => {
                parse_params::<NoParams>(params)?;
                to_value(simctl.device_types().await?)
            }
            "simulator.runtime_download" => {
                let p: DownloadParams = parse_params(params)?;
                runtime_download(ctx, p).await
            }
            "simulator.create" => {
                let p: CreateParams = parse_params(params)?;
                self.create(ctx, &simctl, p).await
            }
            "simulator.lease" => {
                let p: LeaseParams = parse_params(params)?;
                self.lease(ctx, &simctl, p).await
            }
            "simulator.release" => {
                let p: ReleaseParams = parse_params(params)?;
                let id = match (p.lease, p.device) {
                    (Some(id), _) => id,
                    (None, Some(dev)) => {
                        let s = self.pick(&simctl, &dev).await?;
                        hub.lease_of(&s.udid).map(|l| l.id).ok_or_else(|| {
                            JidrError::not_found(format!("{} isn't leased", s.display_name()))
                        })?
                    }
                    (None, None) => {
                        return Err(JidrError::usage("give `lease` or `device`"));
                    }
                };
                self.state.release_simulator(id).await
            }
            "simulator.leases" => {
                parse_params::<NoParams>(params)?;
                to_value(hub.leases())
            }
            "simulator.clone" => {
                let p: CloneParams = parse_params(params)?;
                let s = self.pick(&simctl, &p.device).await?;
                if s.state != SimState::Shutdown {
                    return Err(JidrError::busy(format!(
                        "shut {} down before copying it",
                        s.display_name()
                    )));
                }
                let udid = simctl.clone_device(&s.udid, &p.name).await?;
                hub.remember(&udid, false);
                self.state
                    .emit("simulator.added", json!({ "udid": udid, "name": p.name }));
                to_value(self.pick(&simctl, &udid).await?)
            }
            "simulator.rename" => {
                let p: RenameParams = parse_params(params)?;
                let s = self.pick(&simctl, &p.device).await?;
                simctl.rename(&s.udid, &p.name).await?;
                Ok(json!({ "udid": s.udid, "name": p.name }))
            }
            "simulator.boot" => {
                let p: BootParams = parse_params(params)?;
                let s = self.pick(&simctl, &p.device).await?;
                self.boot(ctx, &simctl, &s, p.force).await?;
                to_value(self.pick(&simctl, &s.udid).await?)
            }
            "simulator.shutdown" => {
                let p: SimParams = parse_params(params)?;
                let s = self.pick(&simctl, &p.device).await?;
                hub.runners.stop(&s.udid).await;
                simctl
                    .clone()
                    .with_timeout(Duration::from_secs(120))
                    .shutdown(&s.udid)
                    .await?;
                Ok(json!({ "udid": s.udid, "state": SimState::Shutdown }))
            }
            "simulator.erase" => {
                let p: ForceParams = parse_params(params)?;
                let s = self.pick(&simctl, &p.device).await?;
                guard_foreign(&s, p.force, "erase")?;
                if s.state.is_booted() {
                    simctl.shutdown(&s.udid).await?;
                }
                simctl.erase(&s.udid).await?;
                Ok(json!({ "erased": s.udid }))
            }
            "simulator.delete" => {
                let p: ForceParams = parse_params(params)?;
                let s = self.pick(&simctl, &p.device).await?;
                guard_foreign(&s, p.force, "delete")?;
                if let Some(l) = &s.lease {
                    return self.state.release_simulator(l.id).await;
                }
                remove(&simctl, hub, &s.udid).await?;
                self.state
                    .emit("simulator.removed", json!({ "udid": s.udid }));
                Ok(json!({ "deleted": s.udid, "name": s.name }))
            }
            "simulator.screenshot" => {
                let p: ScreenshotParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let png = simctl.screenshot_png(&s.udid).await?;
                let path = p.path.unwrap_or_else(|| {
                    jidr_core::paths::capture_path(
                        &jidr_core::paths::capture_dir("Pictures"),
                        &s.name,
                        "png",
                    )
                });
                write_file(&path, &png)?;
                let (w, h) = crate::physical::png_size(&png).unwrap_or((0, 0));
                Ok(json!({ "path": path, "width": w, "height": h }))
            }
            "simulator.record" => {
                let p: RecordParams = parse_params(params)?;
                self.record(ctx, &simctl, p).await
            }
            "simulator.record_stop" => {
                let p: SimParams = parse_params(params)?;
                let s = self.pick(&simctl, &p.device).await?;
                let stop = lock(&hub.recordings).get(&s.udid).cloned();
                match stop {
                    Some(n) => {
                        n.notify_one();
                        Ok(json!({ "stopping": true }))
                    }
                    None => Err(JidrError::not_found(format!(
                        "{} isn't recording",
                        s.display_name()
                    ))),
                }
            }
            "simulator.install" => {
                let p: InstallParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let (app, _tmp) = app_bundle(&p.app).await?;
                simctl.install(&s.udid, &app).await?;
                Ok(json!({ "installed": app, "udid": s.udid }))
            }
            "simulator.uninstall" => {
                let p: BundleParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                simctl.uninstall(&s.udid, &p.bundle).await?;
                Ok(json!({ "uninstalled": p.bundle }))
            }
            "simulator.apps" => {
                let p: SimParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                to_value(simctl.apps(&s.udid).await?)
            }
            "simulator.launch" => {
                let p: LaunchParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let pid = simctl
                    .launch(&s.udid, &p.bundle, &p.args, p.relaunch)
                    .await?;
                Ok(json!({ "launched": p.bundle, "pid": pid }))
            }
            "simulator.terminate" => {
                let p: BundleParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                simctl.terminate(&s.udid, &p.bundle).await?;
                Ok(json!({ "terminated": p.bundle }))
            }
            "simulator.push_file" => {
                let p: PushFileParams = parse_params(params)?;
                if !p.local.exists() {
                    return Err(JidrError::usage(format!(
                        "no such file: {}",
                        p.local.display()
                    )));
                }
                let s = self.booted(&simctl, &p.device).await?;
                let root = simctl.app_container(&s.udid, &p.bundle, "data").await?;
                let remote = p.remote.unwrap_or_else(|| "Documents/".into());
                let dest = inside(&root, &remote)?;
                let dest = if remote.ends_with('/') || dest.is_dir() {
                    dest.join(p.local.file_name().unwrap_or_default())
                } else {
                    dest
                };
                copy_path(&p.local, &dest)?;
                Ok(json!({ "pushed": p.local, "remote": dest }))
            }
            "simulator.pull_file" => {
                let p: PullFileParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let root = simctl.app_container(&s.udid, &p.bundle, "data").await?;
                let src = inside(&root, &p.remote)?;
                if !src.exists() {
                    return Err(JidrError::not_found(format!(
                        "no {} in {}'s data container",
                        p.remote, p.bundle
                    )));
                }
                copy_path(&src, &p.local)?;
                Ok(json!({ "pulled": src, "local": p.local }))
            }
            "simulator.log" => {
                let p: LogParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let last = format!("{}s", p.seconds.unwrap_or(60).clamp(1, 3600));
                let mut args = vec![
                    "spawn", &s.udid, "log", "show", "--last", &last, "--style", "compact",
                ];
                if let Some(pr) = p.predicate.as_deref() {
                    args.extend(["--predicate", pr]);
                }
                let out = simctl
                    .clone()
                    .with_timeout(Duration::from_secs(120))
                    .run(&args)
                    .await?;
                let text = match p.lines {
                    Some(n) => crate::core_provider::tail_lines(&out.stdout, n as usize),
                    None => out.stdout,
                };
                Ok(json!({ "text": text }))
            }
            "simulator.open_url" => {
                let p: UrlParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                if simctl.open_url(&s.udid, &p.url).await? {
                    Ok(json!({ "opened": p.url, "confirmed": true }))
                } else {
                    Ok(json!({
                        "opened": p.url,
                        "confirmed": false,
                        "note": "simctl timed out waiting for the simulator to confirm; it usually opens anyway, so check with a screenshot",
                    }))
                }
            }
            "simulator.location_set" => {
                let p: LocationParams = parse_params(params)?;
                check_coords(p.lat, p.lon)?;
                let s = self.booted(&simctl, &p.device).await?;
                simctl.location_set(&s.udid, p.lat, p.lon).await?;
                Ok(json!({ "lat": p.lat, "lon": p.lon }))
            }
            "simulator.location_route" => {
                let p: RouteParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let points = route_points(&p)?;
                let speed = p.speed_mps.unwrap_or(20.0);
                if !(speed > 0.0 && speed <= 1000.0) {
                    return Err(JidrError::usage("speed_mps must be between 0 and 1000"));
                }
                simctl.location_route(&s.udid, &points, speed, 1.0).await?;
                Ok(json!({ "points": points.len(), "speed_mps": speed }))
            }
            "simulator.location_clear" => {
                let p: SimParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                simctl.location_clear(&s.udid).await?;
                Ok(json!({ "cleared": true }))
            }
            "simulator.appearance" => {
                let p: AppearanceParams = parse_params(params)?;
                if let Some(m) = p.mode.as_deref()
                    && !matches!(m, "light" | "dark")
                {
                    return Err(JidrError::usage("mode is light or dark"));
                }
                let s = self.booted(&simctl, &p.device).await?;
                simctl.ui(&s.udid, "appearance", p.mode.as_deref()).await?;
                let now = simctl.ui(&s.udid, "appearance", None).await?;
                Ok(json!({ "mode": now }))
            }
            "simulator.content_size" => {
                let p: ContentSizeParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                simctl
                    .ui(&s.udid, "content_size", p.size.as_deref())
                    .await?;
                let now = simctl.ui(&s.udid, "content_size", None).await?;
                Ok(json!({ "size": now }))
            }
            "simulator.status_bar" => {
                let p: StatusBarParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                if p.clear {
                    simctl.status_bar_clear(&s.udid).await?;
                    return Ok(json!({ "cleared": true }));
                }
                let bar = StatusBar {
                    time: p.time,
                    data_network: p.data_network,
                    wifi_bars: p.wifi_bars.map(|b| b.min(3)),
                    cellular_bars: p.cellular_bars.map(|b| b.min(4)),
                    operator_name: p.operator_name,
                    battery_state: p.battery_state,
                    battery_level: p.battery_level.map(|b| b.min(100)),
                };
                if bar.is_empty() {
                    return Err(JidrError::usage(
                        "give at least one override, or clear: true",
                    ));
                }
                simctl.status_bar(&s.udid, &bar).await?;
                Ok(json!({ "overridden": true }))
            }
            "simulator.privacy" => {
                let p: PrivacyParams = parse_params(params)?;
                if !matches!(p.action.as_str(), "grant" | "revoke" | "reset") {
                    return Err(JidrError::usage("action is grant, revoke or reset"));
                }
                if p.action != "reset" && p.bundle.is_none() {
                    return Err(JidrError::usage(format!("{} needs bundle", p.action)));
                }
                let s = self.booted(&simctl, &p.device).await?;
                simctl
                    .privacy(&s.udid, &p.action, &p.service, p.bundle.as_deref())
                    .await?;
                Ok(json!({ "action": p.action, "service": p.service, "bundle": p.bundle }))
            }
            "simulator.push_notification" => {
                let p: PushNotificationParams = parse_params(params)?;
                if p.payload.get("aps").is_none_or(|a| !a.is_object()) {
                    return Err(JidrError::usage("payload needs an `aps` object"));
                }
                let s = self.booted(&simctl, &p.device).await?;
                simctl
                    .push(&s.udid, &p.bundle, &p.payload.to_string())
                    .await?;
                Ok(json!({ "sent": true }))
            }
            "simulator.pasteboard" => {
                let p: PasteboardParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                match p.text {
                    Some(t) => {
                        simctl.pbcopy(&s.udid, &t).await?;
                        Ok(json!({ "text": t }))
                    }
                    None => Ok(json!({ "text": simctl.pbpaste(&s.udid).await? })),
                }
            }
            "simulator.add_media" => {
                let p: MediaParams = parse_params(params)?;
                if let Some(missing) = p.files.iter().find(|f| !f.is_file()) {
                    return Err(JidrError::usage(format!(
                        "no such file: {}",
                        missing.display()
                    )));
                }
                if p.files.is_empty() {
                    return Err(JidrError::usage("give at least one file"));
                }
                let s = self.booted(&simctl, &p.device).await?;
                simctl.add_media(&s.udid, &p.files).await?;
                Ok(json!({ "added": p.files.len() }))
            }
            "simulator.trust_cert" => {
                let p: CertParams = parse_params(params)?;
                if !p.cert.is_file() {
                    return Err(JidrError::usage(format!(
                        "no such file: {}",
                        p.cert.display()
                    )));
                }
                let s = self.booted(&simctl, &p.device).await?;
                simctl.trust_cert(&s.udid, &p.cert).await?;
                Ok(json!({ "trusted": p.cert }))
            }
            "simulator.input_prepare" => {
                let p: SimParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let r = self.runner(ctx, &s.udid).await?;
                let screen = r.screen().await?;
                Ok(json!({ "ready": true, "port": r.port, "screen": {
                    "width": screen.width, "height": screen.height, "scale": screen.scale } }))
            }
            "simulator.tap" => {
                let p: TapParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let r = self.runner(ctx, &s.udid).await?;
                let k = r.screen().await?.scale;
                let mut req = json!({ "x": to_points(p.x, k), "y": to_points(p.y, k) });
                if let Some(ms) = p.duration_ms {
                    req["duration_ms"] = json!(ms.clamp(1, 10_000));
                }
                r.call("tap", req).await?;
                Ok(json!({ "tapped": [p.x, p.y] }))
            }
            "simulator.swipe" => {
                let p: SwipeParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let r = self.runner(ctx, &s.udid).await?;
                let k = r.screen().await?.scale;
                let ms = p.duration_ms.unwrap_or(300).clamp(10, 10_000);
                r.call(
                    "swipe",
                    json!({
                        "x1": to_points(p.x1, k), "y1": to_points(p.y1, k),
                        "x2": to_points(p.x2, k), "y2": to_points(p.y2, k),
                        "duration_ms": ms,
                    }),
                )
                .await?;
                Ok(json!({ "swiped": [p.x1, p.y1, p.x2, p.y2], "duration_ms": ms }))
            }
            "simulator.text" => {
                let p: TextParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                if p.text.is_empty() {
                    return Ok(json!({ "typed": 0 }));
                }
                let ids = foreground_candidates(&simctl, &s.udid).await;
                let r = self.runner(ctx, &s.udid).await?;
                let v = r
                    .call("text", json!({ "text": p.text, "bundle_ids": ids }))
                    .await?;
                Ok(json!({ "typed": p.text.chars().count(), "bundle_id": v["bundle_id"] }))
            }
            "simulator.key" => {
                let p: KeyParams = parse_params(params)?;
                let action = crate::sim_runner::key_action(&p.key).ok_or_else(|| {
                    JidrError::usage(format!(
                        "unknown key {:?} for a simulator: home, volume_up, volume_down, enter, backspace, tab",
                        p.key
                    ))
                })?;
                let s = self.booted(&simctl, &p.device).await?;
                let r = self.runner(ctx, &s.udid).await?;
                match action {
                    KeyAction::Button(name) => {
                        r.call("key", json!({ "name": name })).await?;
                    }
                    KeyAction::Text(t) => {
                        let ids = foreground_candidates(&simctl, &s.udid).await;
                        r.call("text", json!({ "text": t, "bundle_ids": ids }))
                            .await?;
                    }
                }
                Ok(json!({ "key": p.key }))
            }
            "simulator.rotate" => {
                let p: RotateParams = parse_params(params)?;
                if !matches!(
                    p.orientation.as_str(),
                    "portrait" | "landscape_left" | "landscape_right" | "portrait_upside_down"
                ) {
                    return Err(JidrError::usage(
                        "orientation is portrait, landscape_left, landscape_right or portrait_upside_down",
                    ));
                }
                let s = self.booted(&simctl, &p.device).await?;
                let r = self.runner(ctx, &s.udid).await?;
                let v = r
                    .call("rotate", json!({ "orientation": p.orientation }))
                    .await?;
                Ok(v)
            }
            "simulator.ui_tree" => {
                let p: SimParams = parse_params(params)?;
                let s = self.booted(&simctl, &p.device).await?;
                let ids = foreground_candidates(&simctl, &s.udid).await;
                let r = self.runner(ctx, &s.udid).await?;
                let k = r.screen().await?.scale;
                let tree = r.call("tree", json!({ "bundle_ids": ids })).await?;
                Ok(crate::sim_runner::tree_in_pixels(tree, k))
            }
            _ => Err(JidrError::usage(format!("unknown method {method}"))),
        }
    }
}

impl SimulatorProvider {
    /// Every simulator in Xcode's set, with what Jidr knows about it.
    async fn list(&self, simctl: &Simctl) -> JidrResult<Vec<Simulator>> {
        let devices = simctl.devices().await?;
        self.hub().set_listing(&devices);
        let runtimes = simctl.runtimes().await.unwrap_or_default();
        let types = simctl.device_types().await.unwrap_or_default();
        Ok(devices
            .iter()
            .map(|d| describe(d, &runtimes, &types, self.hub()))
            .collect())
    }

    async fn pick(&self, simctl: &Simctl, selector: &str) -> JidrResult<Simulator> {
        let all = self.list(simctl).await?;
        pick(&all, selector).cloned()
    }

    /// A booted simulator, or `not_found` saying to boot it.
    async fn booted(&self, simctl: &Simctl, selector: &str) -> JidrResult<Simulator> {
        let s = self.pick(simctl, selector).await?;
        if !s.state.is_booted() {
            return Err(JidrError::not_found(format!(
                "{} isn't booted; boot it with `jidr sim boot {}`",
                s.display_name(),
                s.udid
            ))
            .with_data(json!({ "udid": s.udid, "state": s.state })));
        }
        Ok(s)
    }

    /// Boots `s` after the RAM budget admits it (D42).
    async fn boot(
        &self,
        ctx: &CallCtx,
        simctl: &Simctl,
        s: &Simulator,
        force: bool,
    ) -> JidrResult<()> {
        if s.state.is_booted() {
            return Ok(());
        }
        if s.state == SimState::Unavailable {
            return Err(JidrError::sdk(format!(
                "{} can't boot: {}",
                s.display_name(),
                s.hint.as_deref().unwrap_or("its runtime is missing")
            )));
        }
        self.admit(simctl, &s.name, force).await?;
        ctx.report(Some(0.2), &format!("Booting {}", s.name));
        simctl.boot(&s.udid).await?;
        ctx.report(Some(1.0), "Booted");
        self.state.emit(
            "simulator.changed",
            json!({ "udid": s.udid, "state": SimState::Booted }),
        );
        Ok(())
    }

    /// Refuses to boot past the host RAM budget (D42) unless `force`. Lists
    /// the simulators first: the budget counts the booted ones from the last
    /// listing, and a daemon that just started hasn't listed any yet.
    async fn admit(&self, simctl: &Simctl, name: &str, force: bool) -> JidrResult<()> {
        if force {
            return Ok(());
        }
        if let Ok(devices) = simctl.devices().await {
            self.hub().set_listing(&devices);
        }
        if let Ok(b) = self.state.budget()
            && !b.fits(SIM_EXPECTED_MB)
        {
            return Err(b.refusal(name, SIM_EXPECTED_MB));
        }
        Ok(())
    }

    async fn create(&self, ctx: &CallCtx, simctl: &Simctl, p: CreateParams) -> JidrResult<Value> {
        ctx.report(Some(0.05), "Choosing a runtime");
        let (runtime, device_type) =
            choose(simctl, p.runtime.as_deref(), p.device_type.as_deref()).await?;
        let udid = simctl.create(&p.name, &device_type.id, &runtime.id).await?;
        self.hub().remember(&udid, p.agent_owned);
        self.state
            .emit("simulator.added", json!({ "udid": udid, "name": p.name }));
        let s = self.pick(simctl, &udid).await?;
        if p.boot {
            self.boot(ctx, simctl, &s, p.force).await?;
        }
        to_value(self.pick(simctl, &udid).await?)
    }

    async fn lease(&self, ctx: &CallCtx, simctl: &Simctl, p: LeaseParams) -> JidrResult<Value> {
        ctx.report(Some(0.05), "Choosing a runtime");
        let (runtime, device_type) =
            choose(simctl, p.runtime.as_deref(), p.device_type.as_deref()).await?;
        self.admit(simctl, "a simulator", p.force).await?;
        let id = Uuid::new_v4();
        let name = p
            .name
            .unwrap_or_else(|| format!("Jidr agent {}", &id.simple().to_string()[..6]));
        ctx.report(Some(0.1), &format!("Creating {name} ({})", runtime.name));
        let udid = simctl.create(&name, &device_type.id, &runtime.id).await?;
        let now = Utc::now();
        let (holder, expires_at) = match (p.ttl_s, ctx.connection) {
            (None, Some(conn)) => (SimHolder::Connection { id: conn }, None),
            (ttl, _) => (
                SimHolder::Ttl,
                Some(
                    now + chrono::Duration::from_std(
                        ttl.map(Duration::from_secs).unwrap_or(DEFAULT_TTL),
                    )
                    .unwrap_or_default(),
                ),
            ),
        };
        let lease = SimLease {
            id,
            udid: udid.clone(),
            holder,
            expires_at,
            created_at: now,
        };
        // Recorded before the boot, so a client that disconnects mid-boot
        // still gets its simulator cleaned up.
        self.hub().remember(&udid, true);
        self.hub().add_lease(lease.clone());
        self.state.emit(
            "simulator.leased",
            json!({ "lease": id, "udid": udid, "name": name }),
        );
        let s = self.pick(simctl, &udid).await?;
        if let Err(e) = self.boot(ctx, simctl, &s, true).await {
            let _ = self.state.release_simulator(id).await;
            return Err(e);
        }
        // Ready for input before the agent's first tap; a failure here
        // doesn't fail the lease (screenshots and simctl commands still work).
        let input = match self.runner(ctx, &udid).await {
            Ok(_) => json!({ "ready": true }),
            Err(e) => json!({ "ready": false, "error": e.message }),
        };
        let s = self.pick(simctl, &udid).await?;
        Ok(json!({ "lease": lease, "udid": udid, "simulator": s, "input": input }))
    }

    /// `simulator.record`: `simctl io recordVideo` until stopped, then SIGINT
    /// so it finishes the file.
    async fn record(&self, ctx: &CallCtx, simctl: &Simctl, p: RecordParams) -> JidrResult<Value> {
        let s = self.booted(simctl, &p.device).await?;
        let secs = p.max_seconds.unwrap_or(600).clamp(1, 3600);
        let path = p.path.unwrap_or_else(|| {
            jidr_core::paths::capture_path(&jidr_core::paths::capture_dir("Movies"), &s.name, "mov")
        });
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| JidrError::unexpected(format!("{}: {e}", dir.display())))?;
        }
        let hub = self.hub();
        let stop = Arc::new(Notify::new());
        {
            let mut recs = lock(&hub.recordings);
            if recs.contains_key(&s.udid) {
                return Err(JidrError::busy(format!("{} is already recording", s.name)));
            }
            recs.insert(s.udid.clone(), stop.clone());
        }
        let result = async {
            let mut child = simctl.record(&s.udid, &path)?;
            ctx.report(None, "Recording");
            let started = tokio::time::Instant::now();
            let cancel = ctx.cancel.clone();
            let cancelled = tokio::select! {
                _ = child.wait() => false,
                _ = stop.notified() => false,
                _ = tokio::time::sleep(Duration::from_secs(u64::from(secs))) => false,
                _ = async move {
                    while !cancel.is_cancelled() {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                } => true,
            };
            jidr_simctl::interrupt(&child).await.ok();
            ctx.report(None, "Saving the recording");
            let _ = tokio::time::timeout(Duration::from_secs(30), child.wait()).await;
            let seconds = started.elapsed().as_secs_f64();
            if cancelled {
                let _ = std::fs::remove_file(&path);
                return Err(jidr_core::cancel::cancelled());
            }
            if !path.is_file() {
                return Err(JidrError::sdk("simctl didn't write the recording"));
            }
            Ok(json!({ "path": path, "seconds": (seconds * 10.0).round() / 10.0 }))
        }
        .await;
        lock(&hub.recordings).remove(&s.udid);
        result
    }

    /// Starts the poller once, on the first call.
    fn watch(&self, simctl: &Simctl) {
        {
            let mut w = lock(&self.hub().watching);
            if *w {
                return;
            }
            *w = true;
        }
        let state = self.state.clone();
        let simctl = simctl.clone().with_timeout(Duration::from_secs(30));
        tokio::spawn(async move {
            let mut known: BTreeMap<String, SimState> = BTreeMap::new();
            let mut first = true;
            let mut tick: u64 = 0;
            loop {
                if let Ok(devices) = simctl.devices().await {
                    state.sims.set_listing(&devices);
                    let now: BTreeMap<String, SimState> = devices
                        .iter()
                        .map(|d| (d.udid.clone(), SimState::from_simctl(&d.state, d.available)))
                        .collect();
                    if !first {
                        for (udid, change) in diff(&known, &now) {
                            match change {
                                Change::Added(st) => state
                                    .emit("simulator.added", json!({ "udid": udid, "state": st })),
                                Change::Changed(st) => state.emit(
                                    "simulator.changed",
                                    json!({ "udid": udid, "state": st }),
                                ),
                                Change::Removed => {
                                    // Deleted outside Jidr: forget it and its lease.
                                    state.sims.forget(&udid);
                                    state.emit("simulator.removed", json!({ "udid": udid }));
                                }
                            }
                        }
                    }
                    first = false;
                    known = now;
                }
                tick += 1;
                if tick.is_multiple_of(20) {
                    state.sims.runners.sweep(&state.sims.booted_udids()).await;
                }
                tokio::time::sleep(POLL_EVERY).await;
            }
        });
    }

    /// The simulator's input runner, started (and built) on first use.
    async fn runner(
        &self,
        ctx: &CallCtx,
        udid: &str,
    ) -> JidrResult<Arc<crate::sim_runner::Runner>> {
        if !cfg!(target_os = "macos") {
            return Err(JidrError::sdk(
                "iOS simulators are not supported on this platform",
            ));
        }
        let hub = self.hub();
        let report = |m: &str| ctx.report(None, m);
        hub.runners.get(udid, &hub.cache, &hub.logs, &report).await
    }
}

impl State {
    /// Ends a simulator lease: the simulator is shut down and deleted.
    pub async fn release_simulator(&self, lease: Uuid) -> JidrResult<Value> {
        let l = self
            .sims
            .leases()
            .into_iter()
            .find(|l| l.id == lease)
            .ok_or_else(|| JidrError::not_found(format!("no simulator lease {lease}")))?;
        let simctl = self.sims.simctl().await?;
        remove(&simctl, &self.sims, &l.udid).await?;
        self.emit(
            "simulator.released",
            json!({ "lease": lease, "udid": l.udid }),
        );
        self.emit("simulator.removed", json!({ "udid": l.udid }));
        Ok(json!({ "released": lease, "udid": l.udid }))
    }

    /// One reaper pass over simulator leases as of `now`.
    pub async fn reap_simulators(&self, now: DateTime<Utc>) -> Vec<Uuid> {
        let mut done = Vec::new();
        for l in self.sims.expired(now) {
            match self.release_simulator(l.id).await {
                Ok(_) => done.push(l.id),
                Err(e) => tracing::warn!(lease = %l.id, "releasing a simulator failed: {e}"),
            }
        }
        done
    }
}

/// Shuts down and deletes a simulator Jidr is done with; one that's already
/// gone is fine.
async fn remove(simctl: &Simctl, hub: &SimHub, udid: &str) -> JidrResult<()> {
    hub.runners.stop(udid).await;
    let slow = simctl.clone().with_timeout(Duration::from_secs(120));
    if let Err(e) = slow.shutdown(udid).await
        && e.code != jidr_core::ErrorCode::NotFound
    {
        return Err(e);
    }
    match slow.delete(udid).await {
        Ok(()) => {}
        Err(e) if e.code == jidr_core::ErrorCode::NotFound => {}
        Err(e) => return Err(e),
    }
    hub.forget(udid);
    Ok(())
}

/// Apps that may be in front: the user's apps and the system apps people
/// test against. The runner falls back to SpringBoard (home screen, alerts).
async fn foreground_candidates(simctl: &Simctl, udid: &str) -> Vec<String> {
    let mut ids: Vec<String> = simctl
        .apps(udid)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.kind == "User")
        .map(|a| a.bundle_id)
        .collect();
    ids.extend(
        [
            "com.apple.Preferences",
            "com.apple.mobilesafari",
            "com.apple.MobileSMS",
            "com.apple.mobileslideshow",
            "com.apple.DocumentsApp",
            "com.apple.Maps",
            "com.apple.mobilecal",
            "com.apple.MobileAddressBook",
        ]
        .map(String::from),
    );
    ids
}

fn guard_foreign(s: &Simulator, force: bool, what: &str) -> JidrResult<()> {
    if s.jidr_owned || force {
        return Ok(());
    }
    Err(JidrError::usage(format!(
        "Jidr didn't create {}; pass force to {what} it anyway",
        s.display_name()
    ))
    .with_data(json!({ "udid": s.udid, "jidr_owned": false })))
}

/// The runtime and device type to create with: the newest available iOS
/// runtime that has a regular numbered iPhone, and its newest iPhone Pro,
/// unless named (see [`choose_pair`]).
async fn choose(
    simctl: &Simctl,
    runtime: Option<&str>,
    device_type: Option<&str>,
) -> JidrResult<(SimRuntimeInfo, SimDeviceTypeInfo)> {
    let runtimes = simctl.runtimes().await?;
    let all_types = if runtimes.iter().any(|r| r.device_types.is_empty()) {
        simctl.device_types().await?
    } else {
        Vec::new()
    };
    choose_pair(&runtimes, &all_types, runtime, device_type)
}

/// [`choose`] without `simctl`. A named runtime is used as is. Otherwise the
/// newest available iOS runtime that has the named device type, or, with
/// none named, a regular numbered iPhone: Xcode 27.1's iOS 27.1 runtime only
/// supports "iPhone Duo", whose screenshots are black, and the default must
/// not land on a runtime like that when an older one has an iPhone 17 Pro.
pub(crate) fn choose_pair(
    runtimes: &[jidr_simctl::Runtime],
    all_types: &[SimDeviceTypeInfo],
    runtime: Option<&str>,
    device_type: Option<&str>,
) -> JidrResult<(SimRuntimeInfo, SimDeviceTypeInfo)> {
    let types_of = |rt: &jidr_simctl::Runtime| {
        if rt.device_types.is_empty() {
            all_types.to_vec()
        } else {
            rt.device_types.clone()
        }
    };
    if runtime.is_some() {
        let rt = pick_runtime(runtimes, runtime)?;
        let dt = pick_device_type(&types_of(rt), device_type, &rt.info)?;
        return Ok((rt.info.clone(), dt));
    }
    let mut ios: Vec<&jidr_simctl::Runtime> = runtimes
        .iter()
        .filter(|r| r.info.available && r.info.platform == "iOS")
        .collect();
    ios.sort_by_key(|r| std::cmp::Reverse(jidr_simctl::parse::version_key(&r.info.version)));
    for rt in &ios {
        let types = types_of(rt);
        let fits = match device_type {
            Some(_) => pick_device_type(&types, device_type, &rt.info).is_ok(),
            None => types
                .iter()
                .any(|t| t.family == "iPhone" && model_number(&t.name) > 0),
        };
        if fits {
            let dt = pick_device_type(&types, device_type, &rt.info)?;
            return Ok((rt.info.clone(), dt));
        }
    }
    // Nothing better: the newest iOS runtime as it is, or its error.
    let rt = pick_runtime(runtimes, None)?;
    let dt = pick_device_type(&types_of(rt), device_type, &rt.info)?;
    Ok((rt.info.clone(), dt))
}

pub(crate) fn pick_runtime<'a>(
    runtimes: &'a [jidr_simctl::Runtime],
    want: Option<&str>,
) -> JidrResult<&'a jidr_simctl::Runtime> {
    let available = runtimes.iter().filter(|r| r.info.available);
    let found = match want {
        Some(w) => {
            let w = w.trim();
            let norm = |s: &str| s.to_lowercase().replace(' ', "");
            available.into_iter().find(|r| {
                r.info.id == w
                    || norm(&r.info.name) == norm(w)
                    || r.info.version == w && r.info.platform == "iOS"
            })
        }
        None => available
            .filter(|r| r.info.platform == "iOS")
            .max_by_key(|r| jidr_simctl::parse::version_key(&r.info.version)),
    };
    found.ok_or_else(|| {
        JidrError::not_found(match want {
            Some(w) => format!(
                "no installed simulator runtime {w:?}; `jidr sim runtimes` lists them"
            ),
            None => "no iOS simulator runtime is installed; download one with `jidr sim runtime-download` (about 8 GB) or in Xcode › Settings › Components".into(),
        })
    })
}

/// The first number in a device type name (`iPhone 17 Pro` → 17).
fn model_number(name: &str) -> u32 {
    name.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

pub(crate) fn pick_device_type(
    types: &[SimDeviceTypeInfo],
    want: Option<&str>,
    runtime: &SimRuntimeInfo,
) -> JidrResult<SimDeviceTypeInfo> {
    if let Some(w) = want {
        let lw = w.trim().to_lowercase();
        return types
            .iter()
            .find(|t| t.id == w || t.name.to_lowercase() == lw)
            .cloned()
            .ok_or_else(|| {
                JidrError::not_found(format!(
                    "{} doesn't support a device type {w:?}; `jidr sim device-types` lists them",
                    runtime.name
                ))
            });
    }
    let family = match runtime.platform.as_str() {
        "tvOS" => "Apple TV",
        "watchOS" => "Apple Watch",
        "visionOS" => "Apple Vision",
        _ => "iPhone",
    };
    let of_family: Vec<&SimDeviceTypeInfo> = types.iter().filter(|t| t.family == family).collect();
    let newest = of_family.iter().map(|t| model_number(&t.name)).max();
    let pick = newest.and_then(|n| {
        let same: Vec<&&SimDeviceTypeInfo> = of_family
            .iter()
            .filter(|t| model_number(&t.name) == n)
            .collect();
        same.iter()
            .find(|t| t.name.ends_with(" Pro"))
            .or_else(|| same.iter().min_by_key(|t| t.name.len()))
            .map(|t| (**t).clone())
    });
    pick.or_else(|| types.last().cloned())
        .ok_or_else(|| JidrError::not_found(format!("{} lists no device types", runtime.name)))
}

/// What `simulator.list` returns for one device.
fn describe(
    d: &SimDevice,
    runtimes: &[jidr_simctl::Runtime],
    types: &[SimDeviceTypeInfo],
    hub: &SimHub,
) -> Simulator {
    let rt = runtimes.iter().find(|r| r.info.id == d.runtime_id);
    let owned = hub.owned(&d.udid);
    Simulator {
        udid: d.udid.clone(),
        name: d.name.clone(),
        state: SimState::from_simctl(&d.state, d.available),
        platform: rt.map_or_else(
            || jidr_simctl::parse::platform_of_runtime(&d.runtime_id),
            |r| r.info.platform.clone(),
        ),
        runtime: rt.map_or_else(
            || jidr_simctl::parse::runtime_name_from_id(&d.runtime_id),
            |r| r.info.name.clone(),
        ),
        runtime_id: d.runtime_id.clone(),
        device_type: types
            .iter()
            .chain(runtimes.iter().flat_map(|r| r.device_types.iter()))
            .find(|t| t.id == d.device_type_id)
            .map_or_else(
                || {
                    d.device_type_id
                        .rsplit('.')
                        .next()
                        .unwrap_or_default()
                        .replace('-', " ")
                },
                |t| t.name.clone(),
            ),
        device_type_id: d.device_type_id.clone(),
        jidr_owned: owned.is_some(),
        agent_owned: owned.is_some_and(|o| o.agent),
        lease: hub.lease_of(&d.udid),
        data_path: d.data_path.clone(),
        hint: if d.available {
            None
        } else {
            Some(
                d.availability_error
                    .clone()
                    .unwrap_or_else(|| "its runtime isn't installed".into()),
            )
        },
    }
}

/// A simulator by UDID, or by name when exactly one has it.
pub(crate) fn pick<'a>(all: &'a [Simulator], selector: &str) -> JidrResult<&'a Simulator> {
    let sel = selector.trim();
    if let Some(s) = all.iter().find(|s| s.udid.eq_ignore_ascii_case(sel)) {
        return Ok(s);
    }
    let by_name: Vec<_> = all
        .iter()
        .filter(|s| s.name.eq_ignore_ascii_case(sel))
        .collect();
    match by_name.as_slice() {
        [one] => Ok(one),
        [] => Err(JidrError::not_found(format!(
            "no simulator {sel:?}; `jidr sim list` shows them"
        ))),
        many => Err(JidrError::usage(format!(
            "{} simulators are named {sel:?}; name one by UDID: {}",
            many.len(),
            many.iter()
                .map(|s| format!("{} ({})", s.udid, s.runtime))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    Added(SimState),
    Changed(SimState),
    Removed,
}

pub(crate) fn diff(
    before: &BTreeMap<String, SimState>,
    after: &BTreeMap<String, SimState>,
) -> Vec<(String, Change)> {
    let mut out = Vec::new();
    for (udid, state) in after {
        match before.get(udid) {
            None => out.push((udid.clone(), Change::Added(*state))),
            Some(old) if old != state => out.push((udid.clone(), Change::Changed(*state))),
            Some(_) => {}
        }
    }
    for udid in before.keys() {
        if !after.contains_key(udid) {
            out.push((udid.clone(), Change::Removed));
        }
    }
    out
}

fn route_points(p: &RouteParams) -> JidrResult<Vec<(f64, f64)>> {
    let points: Vec<(f64, f64)> = match &p.file {
        Some(f) => {
            let text = std::fs::read_to_string(f)
                .map_err(|e| JidrError::usage(format!("{}: {e}", f.display())))?;
            jidr_core::track::parse(&text)?
                .into_iter()
                .map(|pt| (pt.lat, pt.lon))
                .collect()
        }
        None => p.points.iter().map(|[lat, lon]| (*lat, *lon)).collect(),
    };
    for (lat, lon) in &points {
        check_coords(*lat, *lon)?;
    }
    if points.len() < 2 {
        return Err(JidrError::usage(
            "a route needs a GPX/KML file or at least two points",
        ));
    }
    Ok(points)
}

fn check_coords(lat: f64, lon: f64) -> JidrResult<()> {
    if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
        Ok(())
    } else {
        Err(JidrError::usage(format!(
            "({lat}, {lon}) isn't a valid position"
        )))
    }
}

/// `remote` inside `root`; `..` and absolute paths are refused.
fn inside(root: &Path, remote: &str) -> JidrResult<PathBuf> {
    let rel = Path::new(remote.trim_start_matches('/'));
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(JidrError::usage("remote paths can't contain `..`"));
    }
    Ok(root.join(rel))
}

/// Copies a file or a folder (recursively), creating parent folders.
fn copy_path(src: &Path, dest: &Path) -> JidrResult<()> {
    let err = |p: &Path, e: std::io::Error| JidrError::unexpected(format!("{}: {e}", p.display()));
    if src.is_dir() {
        std::fs::create_dir_all(dest).map_err(|e| err(dest, e))?;
        for entry in std::fs::read_dir(src).map_err(|e| err(src, e))? {
            let entry = entry.map_err(|e| err(src, e))?;
            copy_path(&entry.path(), &dest.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| err(dir, e))?;
    }
    std::fs::copy(src, dest).map_err(|e| err(src, e)).map(drop)
}

/// The `.app` to install: the path itself, or the one inside a `.zip`
/// (unpacked with `ditto` into a temp folder kept alive by the guard).
async fn app_bundle(path: &Path) -> JidrResult<(PathBuf, Option<tempfile::TempDir>)> {
    if path.is_dir() && path.extension().is_some_and(|e| e == "app") {
        return Ok((path.to_path_buf(), None));
    }
    let is_zip = path.is_file()
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if !is_zip {
        return Err(JidrError::usage(format!(
            "{} isn't a simulator build: give an .app folder or a .zip holding one (App Store .ipa files run on devices only)",
            path.display()
        )));
    }
    let tmp = tempfile::Builder::new()
        .prefix("jidr-app")
        .tempdir()
        .map_err(|e| JidrError::unexpected(format!("temp dir: {e}")))?;
    let status = tokio::process::Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(path)
        .arg(tmp.path())
        .status()
        .await
        .map_err(|e| JidrError::unexpected(format!("ditto: {e}")))?;
    if !status.success() {
        return Err(JidrError::usage(format!("can't unzip {}", path.display())));
    }
    let app = find_app(tmp.path(), 3)
        .ok_or_else(|| JidrError::usage(format!("{} holds no .app", path.display())))?;
    Ok((app, Some(tmp)))
}

fn find_app(dir: &Path, depth: u32) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.extension().is_some_and(|x| x == "app") {
                return Some(p);
            }
            if !p.file_name().is_some_and(|n| n == "__MACOSX") {
                subdirs.push(p);
            }
        }
    }
    if depth == 0 {
        return None;
    }
    subdirs.into_iter().find_map(|d| find_app(&d, depth - 1))
}

/// `simulator.runtime_download`: `xcodebuild -downloadPlatform`, with its
/// progress lines as job progress.
async fn runtime_download(ctx: &CallCtx, p: DownloadParams) -> JidrResult<Value> {
    use tokio::io::AsyncBufReadExt;
    let platform = match p.platform.as_deref().map(str::to_lowercase).as_deref() {
        None | Some("ios") => "iOS",
        Some("tvos") => "tvOS",
        Some("watchos") => "watchOS",
        Some("visionos") | Some("xros") => "visionOS",
        Some(other) => {
            return Err(JidrError::usage(format!(
                "unknown platform {other:?}: iOS, tvOS, watchOS or visionOS"
            )));
        }
    };
    if !cfg!(target_os = "macos") {
        return Err(JidrError::sdk(
            "iOS simulators are not supported on this platform",
        ));
    }
    let mut cmd = tokio::process::Command::new("/usr/bin/xcrun");
    cmd.args(["xcodebuild", "-downloadPlatform", platform]);
    if let Some(v) = &p.version {
        cmd.args(["-buildVersion", v]);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| JidrError::sdk(format!("cannot run xcodebuild: {e}")))?;
    ctx.report(
        None,
        &format!("Downloading the {platform} simulator runtime"),
    );
    let mut tail = Vec::new();
    if let Some(out) = child.stdout.take() {
        let mut lines = tokio::io::BufReader::new(out).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) => {
                        let l = l.trim().to_string();
                        if !l.is_empty() {
                            ctx.report(percent(&l), &l);
                            tail.push(l);
                            if tail.len() > 20 {
                                tail.remove(0);
                            }
                        }
                    }
                    _ => break,
                },
                _ = wait_cancel(&ctx.cancel) => {
                    let _ = child.kill().await;
                    return Err(jidr_core::cancel::cancelled());
                }
            }
        }
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| JidrError::unexpected(format!("xcodebuild: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if err.is_empty() { tail.join("\n") } else { err };
        let hint = if msg.contains("license") {
            "; accept Xcode's licence yourself first (`sudo xcodebuild -license`)"
        } else {
            ""
        };
        return Err(JidrError::sdk(format!(
            "xcodebuild -downloadPlatform {platform} failed: {msg}{hint}"
        )));
    }
    Ok(json!({ "platform": platform, "version": p.version }))
}

/// `12.3%` in an xcodebuild progress line.
fn percent(line: &str) -> Option<f32> {
    let i = line.find('%')?;
    let num: String = line[..i]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    num.parse::<f32>().ok().map(|p| (p / 100.0).clamp(0.0, 1.0))
}

async fn wait_cancel(c: &jidr_core::Cancel) {
    while !c.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> JidrResult<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| JidrError::unexpected(format!("{}: {e}", dir.display())))?;
    }
    std::fs::write(path, bytes)
        .map_err(|e| JidrError::unexpected(format!("{}: {e}", path.display())))
}

fn to_value<T: Serialize>(v: T) -> JidrResult<Value> {
    serde_json::to_value(v).map_err(|e| JidrError::unexpected(format!("encoding result: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim(udid: &str, name: &str) -> Simulator {
        Simulator {
            udid: udid.into(),
            name: name.into(),
            state: SimState::Shutdown,
            platform: "iOS".into(),
            runtime: "iOS 26.1".into(),
            runtime_id: "r".into(),
            device_type: name.into(),
            device_type_id: "t".into(),
            jidr_owned: false,
            agent_owned: false,
            lease: None,
            data_path: None,
            hint: None,
        }
    }

    #[test]
    fn simulators_are_picked_by_udid_or_a_unique_name() {
        let all = vec![
            sim("AAAA", "iPhone 17 Pro"),
            sim("BBBB", "iPad Air"),
            sim("CCCC", "iPad Air"),
        ];
        assert_eq!(pick(&all, "aaaa").unwrap().udid, "AAAA");
        assert_eq!(pick(&all, "iphone 17 pro").unwrap().udid, "AAAA");
        assert_eq!(
            pick(&all, "iPad Air").unwrap_err().code,
            jidr_core::ErrorCode::Usage
        );
        assert_eq!(
            pick(&all, "nope").unwrap_err().code,
            jidr_core::ErrorCode::NotFound
        );
    }

    #[test]
    fn foreign_simulators_need_force() {
        let mut s = sim("AAAA", "x");
        assert!(guard_foreign(&s, false, "delete").is_err());
        assert!(guard_foreign(&s, true, "delete").is_ok());
        s.jidr_owned = true;
        assert!(guard_foreign(&s, false, "delete").is_ok());
    }

    fn dt(name: &str) -> SimDeviceTypeInfo {
        SimDeviceTypeInfo {
            id: format!("id.{name}"),
            name: name.into(),
            family: if name.starts_with("iPad") {
                "iPad"
            } else {
                "iPhone"
            }
            .into(),
        }
    }

    fn rt(platform: &str, version: &str) -> jidr_simctl::Runtime {
        jidr_simctl::Runtime {
            info: SimRuntimeInfo {
                id: format!(
                    "com.apple.CoreSimulator.SimRuntime.{platform}-{}",
                    version.replace('.', "-")
                ),
                name: format!("{platform} {version}"),
                platform: platform.into(),
                version: version.into(),
                build: "x".into(),
                available: true,
            },
            device_types: Vec::new(),
        }
    }

    #[test]
    fn the_newest_ios_runtime_and_iphone_pro_are_the_default() {
        let rts = vec![rt("iOS", "18.5"), rt("iOS", "26.1"), rt("tvOS", "27.0")];
        assert_eq!(pick_runtime(&rts, None).unwrap().info.version, "26.1");
        assert_eq!(
            pick_runtime(&rts, Some("18.5")).unwrap().info.version,
            "18.5"
        );
        assert_eq!(
            pick_runtime(&rts, Some("ios 18.5")).unwrap().info.version,
            "18.5"
        );
        assert!(pick_runtime(&rts, Some("iOS 9")).is_err());
        assert!(pick_runtime(&[], None).is_err());

        let types = vec![
            dt("iPhone 16 Pro"),
            dt("iPhone 17"),
            dt("iPhone 17 Pro Max"),
            dt("iPhone 17 Pro"),
            dt("iPad Pro 13-inch (M5)"),
        ];
        let info = &rts[1].info;
        assert_eq!(
            pick_device_type(&types, None, info).unwrap().name,
            "iPhone 17 Pro"
        );
        assert_eq!(
            pick_device_type(&types, Some("iphone 16 pro"), info)
                .unwrap()
                .name,
            "iPhone 16 Pro"
        );
        assert!(pick_device_type(&types, Some("Pixel"), info).is_err());
    }

    fn rt_with(platform: &str, version: &str, types: &[&str]) -> jidr_simctl::Runtime {
        let mut r = rt(platform, version);
        r.device_types = types.iter().map(|t| dt(t)).collect();
        r
    }

    // Regression: Xcode 27.1's iOS 27.1 runtime supports only "iPhone Duo"
    // (black screenshots), and the default lease landed on it.
    #[test]
    fn the_default_skips_a_runtime_without_a_numbered_iphone() {
        let rts = vec![
            rt_with("iOS", "27.1", &["iPhone Duo"]),
            rt_with(
                "iOS",
                "26.1",
                &["iPhone 17", "iPhone 17 Pro", "iPad Air 11-inch (M3)"],
            ),
            rt_with("iOS", "18.5", &["iPhone 16 Pro"]),
        ];
        let (r, d) = choose_pair(&rts, &[], None, None).unwrap();
        assert_eq!(
            (r.version.as_str(), d.name.as_str()),
            ("26.1", "iPhone 17 Pro")
        );
        // Asking for it, or for its runtime, still gets it.
        let (r, d) = choose_pair(&rts, &[], None, Some("iPhone Duo")).unwrap();
        assert_eq!(
            (r.version.as_str(), d.name.as_str()),
            ("27.1", "iPhone Duo")
        );
        let (r, d) = choose_pair(&rts, &[], Some("27.1"), None).unwrap();
        assert_eq!(
            (r.version.as_str(), d.name.as_str()),
            ("27.1", "iPhone Duo")
        );
        // A named type picks the newest runtime that has it.
        let (r, _) = choose_pair(&rts, &[], None, Some("iPhone 16 Pro")).unwrap();
        assert_eq!(r.version, "18.5");
        // Only the odd runtime: it's used rather than failing.
        let (r, d) = choose_pair(&rts[..1], &[], None, None).unwrap();
        assert_eq!(
            (r.version.as_str(), d.name.as_str()),
            ("27.1", "iPhone Duo")
        );
    }

    // Regression: the first lease after the daemon started skipped the RAM
    // budget, because no simulator listing had happened yet.
    #[tokio::test]
    async fn admission_counts_simulators_booted_before_the_first_listing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let td = crate::testing::TestDaemon::start_with_memory(
            dir.path(),
            jidr_sdk::host::HostMemory {
                total_mb: 8192,
                available_mb: 8192,
            },
        )
        .await
        .unwrap();
        // A simctl that reports one booted simulator, someone else's.
        let fake = dir.path().join("simctl");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
echo '{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-26-1":[{"udid":"7883BE7C-9443-46AF-A2AB-40AB0A0D6FE4","name":"Busy","state":"Booted","isAvailable":true,"deviceTypeIdentifier":"x"}]}}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let simctl = Simctl::new(&fake);
        let provider = SimulatorProvider::new(td.daemon.state().clone());
        assert!(provider.hub().booted().is_empty(), "nothing listed yet");
        // 75% of 8 GB is 6 GB: the booted one (4 GB) leaves no room for another.
        let e = provider
            .admit(&simctl, "a simulator", false)
            .await
            .unwrap_err();
        assert_eq!(e.code, jidr_core::ErrorCode::ResourceExhausted, "{e}");
        assert!(
            provider.admit(&simctl, "a simulator", true).await.is_ok(),
            "force"
        );
    }

    #[test]
    fn listings_diff_into_events() {
        let mut before = BTreeMap::new();
        before.insert("A".to_string(), SimState::Shutdown);
        before.insert("B".to_string(), SimState::Booted);
        let mut after = BTreeMap::new();
        after.insert("A".to_string(), SimState::Booted);
        after.insert("C".to_string(), SimState::Shutdown);
        assert_eq!(
            diff(&before, &after),
            vec![
                ("A".into(), Change::Changed(SimState::Booted)),
                ("C".into(), Change::Added(SimState::Shutdown)),
                ("B".into(), Change::Removed),
            ]
        );
    }

    #[test]
    fn leases_of_a_closed_connection_expire_after_grace() {
        let dir = tempfile::tempdir().unwrap();
        let hub = SimHub::load(
            dir.path().join("simulators.json"),
            dir.path().join("cache"),
            dir.path().join("logs"),
        );
        let now = Utc::now();
        hub.add_lease(SimLease {
            id: Uuid::new_v4(),
            udid: "A".into(),
            holder: SimHolder::Connection { id: 7 },
            expires_at: None,
            created_at: now,
        });
        assert!(hub.expired(now + chrono::Duration::days(1)).is_empty());
        hub.disconnected(8, now);
        assert!(hub.expired(now + chrono::Duration::days(1)).is_empty());
        hub.disconnected(7, now);
        assert!(hub.expired(now + chrono::Duration::seconds(60)).is_empty());
        assert_eq!(hub.expired(now + chrono::Duration::seconds(121)).len(), 1);

        // A restarted daemon gives leftover connection leases the grace period.
        let again = SimHub::load(
            dir.path().join("simulators.json"),
            dir.path().join("cache"),
            dir.path().join("logs"),
        );
        let l = again.leases().pop().unwrap();
        assert_eq!(l.holder, SimHolder::Ttl);
        assert!(l.expires_at.is_some());
    }

    #[test]
    fn container_paths_stay_inside() {
        let root = Path::new("/c");
        assert_eq!(
            inside(root, "Documents/a.txt").unwrap(),
            Path::new("/c/Documents/a.txt")
        );
        assert_eq!(
            inside(root, "/Library/x").unwrap(),
            Path::new("/c/Library/x")
        );
        assert!(inside(root, "../etc/passwd").is_err());
    }

    #[test]
    fn progress_percent() {
        assert_eq!(
            percent("Downloading iOS 26.1: 42.5% (3.4 GB of 8 GB)"),
            Some(0.425)
        );
        assert_eq!(percent("Preparing"), None);
    }

    #[test]
    fn routes_need_two_valid_points() {
        let p = |points: Vec<[f64; 2]>| RouteParams {
            device: "x".into(),
            file: None,
            points,
            speed_mps: None,
        };
        assert!(route_points(&p(vec![[1.0, 2.0]])).is_err());
        assert!(route_points(&p(vec![[1.0, 2.0], [95.0, 2.0]])).is_err());
        assert_eq!(
            route_points(&p(vec![[1.0, 2.0], [3.0, 4.0]]))
                .unwrap()
                .len(),
            2
        );
    }
}
