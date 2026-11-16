//! Parsers for `simctl`'s `-j` JSON (and the `plutil` JSON of `listapps`).
//! Pure functions, so they're tested offline against captured output.

use std::path::PathBuf;

use jidr_core::{JidrError, JidrResult, SimDeviceTypeInfo, SimRuntimeInfo};
use serde::Deserialize;
use serde_json::Value;

/// A simulator as `simctl list devices -j` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimDevice {
    pub udid: String,
    pub name: String,
    /// `Booted`, `Shutdown`, `Shutting Down`, …
    pub state: String,
    pub available: bool,
    pub runtime_id: String,
    pub device_type_id: String,
    pub data_path: Option<PathBuf>,
    pub availability_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDevice {
    udid: String,
    name: String,
    #[serde(default)]
    state: String,
    #[serde(default = "yes")]
    is_available: bool,
    #[serde(default)]
    device_type_identifier: String,
    #[serde(default)]
    data_path: Option<PathBuf>,
    #[serde(default)]
    availability_error: Option<String>,
}

fn yes() -> bool {
    true
}

fn bad(what: &str, e: impl std::fmt::Display) -> JidrError {
    JidrError::sdk(format!("can't read simctl's {what}: {e}"))
}

/// `simctl list devices -j`: `{"devices": {"<runtime id>": [device…]}}`.
pub fn devices(json: &str) -> JidrResult<Vec<SimDevice>> {
    #[derive(Deserialize)]
    struct List {
        devices: std::collections::BTreeMap<String, Vec<RawDevice>>,
    }
    let list: List = serde_json::from_str(json).map_err(|e| bad("device list", e))?;
    let mut out = Vec::new();
    for (runtime, devs) in list.devices {
        for d in devs {
            out.push(SimDevice {
                udid: d.udid,
                name: d.name,
                state: d.state,
                available: d.is_available,
                runtime_id: runtime.clone(),
                device_type_id: d.device_type_identifier,
                data_path: d.data_path,
                availability_error: d.availability_error,
            });
        }
    }
    Ok(out)
}

/// A runtime with the device types it supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    pub info: SimRuntimeInfo,
    pub device_types: Vec<SimDeviceTypeInfo>,
}

/// `simctl list runtimes -j`.
pub fn runtimes(json: &str) -> JidrResult<Vec<Runtime>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawType {
        identifier: String,
        name: String,
        #[serde(default)]
        product_family: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawRuntime {
        identifier: String,
        name: String,
        #[serde(default)]
        platform: Option<String>,
        #[serde(default)]
        version: String,
        #[serde(default)]
        buildversion: String,
        #[serde(default = "yes")]
        is_available: bool,
        #[serde(default)]
        supported_device_types: Vec<RawType>,
    }
    #[derive(Deserialize)]
    struct List {
        runtimes: Vec<RawRuntime>,
    }
    let list: List = serde_json::from_str(json).map_err(|e| bad("runtime list", e))?;
    Ok(list
        .runtimes
        .into_iter()
        .map(|r| {
            let platform = r
                .platform
                .unwrap_or_else(|| platform_of_runtime(&r.identifier));
            Runtime {
                info: SimRuntimeInfo {
                    id: r.identifier,
                    name: r.name,
                    platform,
                    version: r.version,
                    build: r.buildversion,
                    available: r.is_available,
                },
                device_types: r
                    .supported_device_types
                    .into_iter()
                    .map(|t| SimDeviceTypeInfo {
                        id: t.identifier,
                        name: t.name,
                        family: t.product_family,
                    })
                    .collect(),
            }
        })
        .collect())
}

/// `simctl list devicetypes -j`.
pub fn device_types(json: &str) -> JidrResult<Vec<SimDeviceTypeInfo>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawType {
        identifier: String,
        name: String,
        #[serde(default)]
        product_family: String,
    }
    #[derive(Deserialize)]
    struct List {
        devicetypes: Vec<RawType>,
    }
    let list: List = serde_json::from_str(json).map_err(|e| bad("device type list", e))?;
    Ok(list
        .devicetypes
        .into_iter()
        .map(|t| SimDeviceTypeInfo {
            id: t.identifier,
            name: t.name,
            family: t.product_family,
        })
        .collect())
}

/// `iOS` from `com.apple.CoreSimulator.SimRuntime.iOS-26-1`; CoreSimulator
/// calls visionOS `xrOS`.
pub fn platform_of_runtime(id: &str) -> String {
    let last = id.rsplit('.').next().unwrap_or(id);
    let name = last.split('-').next().unwrap_or(last);
    match name {
        "xrOS" => "visionOS".into(),
        "" => "iOS".into(),
        other => other.into(),
    }
}

/// `iOS 26.1` from `com.apple.CoreSimulator.SimRuntime.iOS-26-1`, for a
/// runtime `list runtimes` doesn't know any more.
pub fn runtime_name_from_id(id: &str) -> String {
    let last = id.rsplit('.').next().unwrap_or(id);
    let mut parts = last.split('-');
    let platform = platform_of_runtime(id);
    let _ = parts.next();
    let version: Vec<&str> = parts.collect();
    if version.is_empty() {
        platform
    } else {
        format!("{platform} {}", version.join("."))
    }
}

/// An installed app (`simctl listapps`, converted to JSON by `plutil`).
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SimApp {
    pub bundle_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `User` or `System`.
    pub kind: String,
}

/// `simctl listapps` after `plutil -convert json`: an object keyed by bundle id.
pub fn apps(json: &str) -> JidrResult<Vec<SimApp>> {
    let v: Value = serde_json::from_str(json).map_err(|e| bad("app list", e))?;
    let Some(map) = v.as_object() else {
        return Err(bad("app list", "not an object"));
    };
    let s = |a: &Value, k: &str| a.get(k).and_then(Value::as_str).map(str::to_string);
    let mut out: Vec<SimApp> = map
        .iter()
        .map(|(id, a)| SimApp {
            bundle_id: s(a, "CFBundleIdentifier").unwrap_or_else(|| id.clone()),
            name: s(a, "CFBundleDisplayName")
                .or_else(|| s(a, "CFBundleName"))
                .unwrap_or_else(|| id.clone()),
            version: s(a, "CFBundleShortVersionString"),
            kind: s(a, "ApplicationType").unwrap_or_else(|| "User".into()),
        })
        .collect();
    out.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id));
    Ok(out)
}

/// Compares dotted versions numerically (`26.1` > `18.5` > `9.0`).
pub fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICES: &str = r#"{"devices":{
      "com.apple.CoreSimulator.SimRuntime.iOS-26-1":[
        {"dataPath":"/Users/me/Library/Developer/CoreSimulator/Devices/5CB5/data","udid":"5CB5","isAvailable":true,
         "deviceTypeIdentifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro","state":"Shutdown","name":"iPhone 17 Pro"},
        {"udid":"7883","isAvailable":true,"deviceTypeIdentifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro-Max","state":"Booted","name":"iPhone 17 Pro Max"}],
      "com.apple.CoreSimulator.SimRuntime.iOS-17-0":[
        {"udid":"OLD1","isAvailable":false,"availabilityError":"runtime profile not found","deviceTypeIdentifier":"x","state":"Shutdown","name":"Old"}],
      "com.apple.CoreSimulator.SimRuntime.xrOS-2-5":[]
    }}"#;

    #[test]
    fn device_list() {
        let d = devices(DEVICES).unwrap();
        assert_eq!(d.len(), 3);
        let booted = d.iter().find(|x| x.udid == "7883").unwrap();
        assert_eq!(booted.state, "Booted");
        assert_eq!(
            booted.runtime_id,
            "com.apple.CoreSimulator.SimRuntime.iOS-26-1"
        );
        let old = d.iter().find(|x| x.udid == "OLD1").unwrap();
        assert!(!old.available);
        assert_eq!(
            old.availability_error.as_deref(),
            Some("runtime profile not found")
        );
        assert!(devices("not json").is_err());
    }

    #[test]
    fn runtime_list() {
        let r = runtimes(
            r#"{"runtimes":[{"isAvailable":true,"version":"26.1","buildversion":"23B86","platform":"iOS",
            "identifier":"com.apple.CoreSimulator.SimRuntime.iOS-26-1","name":"iOS 26.1",
            "supportedDeviceTypes":[{"name":"iPhone 17 Pro","productFamily":"iPhone","identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"}]},
            {"version":"2.5","buildversion":"22O1","identifier":"com.apple.CoreSimulator.SimRuntime.xrOS-2-5","name":"visionOS 2.5"}]}"#,
        )
        .unwrap();
        assert_eq!(r[0].info.platform, "iOS");
        assert_eq!(r[0].device_types[0].family, "iPhone");
        assert_eq!(r[1].info.platform, "visionOS");
        assert!(r[1].info.available);
    }

    #[test]
    fn runtime_ids() {
        assert_eq!(
            platform_of_runtime("com.apple.CoreSimulator.SimRuntime.watchOS-11-5"),
            "watchOS"
        );
        assert_eq!(
            runtime_name_from_id("com.apple.CoreSimulator.SimRuntime.iOS-26-1"),
            "iOS 26.1"
        );
        assert_eq!(
            runtime_name_from_id("com.apple.CoreSimulator.SimRuntime.xrOS-2-5"),
            "visionOS 2.5"
        );
    }

    #[test]
    fn app_list() {
        let a = apps(
            r#"{"com.example.app":{"CFBundleIdentifier":"com.example.app","CFBundleName":"Ex","CFBundleShortVersionString":"1.2","ApplicationType":"User"},
               "com.apple.Bridge":{"CFBundleDisplayName":"Watch","ApplicationType":"System"}}"#,
        )
        .unwrap();
        assert_eq!(a[0].bundle_id, "com.apple.Bridge");
        assert_eq!(a[0].name, "Watch");
        assert_eq!(a[1].version.as_deref(), Some("1.2"));
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(version_key("26.1") > version_key("18.5"));
        assert!(version_key("18.10") > version_key("18.5"));
    }
}
