//! iOS, tvOS, watchOS and visionOS simulators on macOS (`simulator.*`, D50):
//! what `simulator.list`, `simulator.details` and `simulator.lease` return.
//! Simulators aren't Jidr devices: CoreSimulator owns them (Xcode's own
//! device set), Jidr names them by UDID and only deletes the ones it created.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A simulator's state, from `simctl list devices`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SimState {
    #[default]
    Shutdown,
    Booting,
    Booted,
    ShuttingDown,
    /// The runtime is missing or the device type isn't supported any more.
    Unavailable,
}

impl SimState {
    /// Maps `simctl`'s state word (`Booted`, `Shutting Down`, …).
    pub fn from_simctl(state: &str, available: bool) -> Self {
        if !available {
            return Self::Unavailable;
        }
        match state.trim() {
            "Booted" => Self::Booted,
            "Booting" | "Creating" => Self::Booting,
            "Shutting Down" => Self::ShuttingDown,
            _ => Self::Shutdown,
        }
    }

    pub fn is_booted(self) -> bool {
        self == Self::Booted
    }
}

/// Who holds a simulator lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SimHolder {
    /// The client connection that took it (an agent's `jidr mcp`), until it
    /// closes plus 2 minutes.
    Connection { id: u64 },
    /// A fixed lifetime (the CLI's default is 2 hours).
    Ttl,
}

/// A simulator lease: a fresh simulator for one agent session, shut down and
/// deleted when the lease ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SimLease {
    pub id: Uuid,
    /// The leased simulator's UDID.
    pub udid: String,
    pub holder: SimHolder,
    /// When the reaper releases it; `None` while its connection is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// One simulator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Simulator {
    /// What every `simulator.*` command takes as `device`.
    pub udid: String,
    pub name: String,
    pub state: SimState,
    /// `iOS`, `tvOS`, `watchOS` or `visionOS`.
    pub platform: String,
    /// e.g. `iOS 26.1`.
    pub runtime: String,
    /// e.g. `com.apple.CoreSimulator.SimRuntime.iOS-26-1`.
    pub runtime_id: String,
    /// e.g. `iPhone 17 Pro`.
    pub device_type: String,
    pub device_type_id: String,
    /// Created by Jidr (only these are deleted without `force`).
    pub jidr_owned: bool,
    /// Created by an agent (`simulator.lease`, or `simulator.create` over MCP):
    /// agents may erase and delete it without `--allow-destructive` (D5).
    pub agent_owned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<SimLease>,
    /// The device's folder on this Mac (`…/CoreSimulator/Devices/<udid>/data`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_path: Option<std::path::PathBuf>,
    /// Why it's unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Simulator {
    /// `iPhone 17 Pro (iOS 26.1)`.
    pub fn display_name(&self) -> String {
        format!("{} ({})", self.name, self.runtime)
    }
}

/// `simulator.connect_info`: what a client needs to reach a booted simulator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SimConnectInfo {
    pub udid: String,
    pub name: String,
    pub platform: String,
}

/// An installed simulator runtime (`simctl list runtimes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SimRuntimeInfo {
    pub id: String,
    /// e.g. `iOS 26.1`.
    pub name: String,
    pub platform: String,
    pub version: String,
    pub build: String,
    pub available: bool,
}

/// A device type a runtime supports (`simctl list devicetypes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SimDeviceTypeInfo {
    pub id: String,
    /// e.g. `iPhone 17 Pro`.
    pub name: String,
    /// `iPhone`, `iPad`, `Apple TV`, `Apple Watch`, `Apple Vision`.
    pub family: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simctl_states_map() {
        assert_eq!(SimState::from_simctl("Booted", true), SimState::Booted);
        assert_eq!(
            SimState::from_simctl("Shutting Down", true),
            SimState::ShuttingDown
        );
        assert_eq!(SimState::from_simctl("Shutdown", true), SimState::Shutdown);
        assert_eq!(
            SimState::from_simctl("Booted", false),
            SimState::Unavailable
        );
        assert_eq!(
            serde_json::to_value(SimState::ShuttingDown).unwrap(),
            "shutting_down"
        );
    }
}
