//! `jidr sim …`: iOS simulators over `simctl` (D50), thin calls over the
//! `simulator.*` commands.

use jidr_client::Client;
use jidr_core::{JidrError, JidrResult};
use serde_json::{Value, json};

use crate::cli::{SimCmd, parse_duration};
use crate::exec;
use crate::follow;
use crate::output::{Out, table};

fn s(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn abs(p: std::path::PathBuf) -> std::path::PathBuf {
    std::path::absolute(&p).unwrap_or(p)
}

/// `jidr`, `agent`, `lease` or nothing.
fn owner(v: &Value) -> String {
    if !v["lease"].is_null() {
        "lease".into()
    } else if v["agent_owned"] == true {
        "agent".into()
    } else if v["jidr_owned"] == true {
        "jidr".into()
    } else {
        String::new()
    }
}

fn sims(v: &Value) -> String {
    let rows: Vec<Vec<String>> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter(|x| x["state"] != "unavailable")
        .map(|x| {
            vec![
                s(x, "udid"),
                s(x, "state"),
                s(x, "name"),
                s(x, "runtime"),
                owner(x),
            ]
        })
        .collect();
    if rows.is_empty() {
        return "No simulators. Create one with `jidr sim create <name>` or `jidr sim lease`."
            .into();
    }
    table(&["UDID", "STATE", "NAME", "RUNTIME", "JIDR"], rows)
}

fn sim(v: &Value) -> String {
    let mut lines = vec![
        format!("UDID:     {}", s(v, "udid")),
        format!("Name:     {}", s(v, "name")),
        format!("State:    {}", s(v, "state")),
        format!("Runtime:  {}", s(v, "runtime")),
        format!("Type:     {}", s(v, "device_type")),
    ];
    let who = owner(v);
    if !who.is_empty() {
        lines.push(format!("Jidr:     {who}"));
    }
    if let Some(l) = v["lease"].as_object() {
        let until = l
            .get("expires_at")
            .and_then(Value::as_str)
            .unwrap_or("while its connection is open");
        lines.push(format!(
            "Lease:    {} (until {until})",
            l.get("id").and_then(Value::as_str).unwrap_or("?")
        ));
    }
    if let Some(h) = v["hint"].as_str() {
        lines.push(format!("Note:     {h}"));
    }
    lines.join("\n")
}

pub async fn run(c: &Client, out: Out, cmd: SimCmd) -> JidrResult<()> {
    match cmd {
        SimCmd::List => {
            let v = exec(c, out, "simulator.list", json!({})).await?;
            out.ok(&v, sims);
        }
        SimCmd::Details { sim: d } => {
            let v = exec(c, out, "simulator.details", json!({ "device": d })).await?;
            out.ok(&v, sim);
        }
        SimCmd::Runtimes => {
            let v = exec(c, out, "simulator.runtimes", json!({})).await?;
            out.ok(&v, |v| {
                let rows = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| vec![s(r, "name"), s(r, "build"), s(r, "id")])
                    .collect();
                table(&["RUNTIME", "BUILD", "ID"], rows)
            });
        }
        SimCmd::DeviceTypes => {
            let v = exec(c, out, "simulator.device_types", json!({})).await?;
            out.ok(&v, |v| {
                let rows = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| vec![s(t, "name"), s(t, "family"), s(t, "id")])
                    .collect();
                table(&["NAME", "FAMILY", "ID"], rows)
            });
        }
        SimCmd::RuntimeDownload { platform, version } => {
            let v = exec(
                c,
                out,
                "simulator.runtime_download",
                json!({ "platform": platform, "version": version }),
            )
            .await?;
            out.ok(&v, |_| {
                format!("Downloaded the {platform} simulator runtime.")
            });
        }
        SimCmd::Create {
            name,
            device_type,
            runtime,
            boot,
        } => {
            let v = exec(
                c,
                out,
                "simulator.create",
                json!({ "name": name, "device_type": device_type, "runtime": runtime, "boot": boot }),
            )
            .await?;
            out.ok(&v, |v| {
                format!("Created {} ({}).", s(v, "name"), s(v, "udid"))
            });
        }
        SimCmd::Lease {
            device_type,
            runtime,
            name,
            ttl,
            force,
        } => {
            let ttl_s = parse_duration(&ttl).map_err(JidrError::usage)?;
            let v = exec(
                c,
                out,
                "simulator.lease",
                json!({ "device_type": device_type, "runtime": runtime, "name": name, "ttl_s": ttl_s, "force": force }),
            )
            .await?;
            out.ok(&v, |v| s(v, "udid"));
        }
        SimCmd::Release { lease_or_sim } => {
            let p = match uuid_lease(c, &lease_or_sim).await {
                Some(id) => json!({ "lease": id }),
                None => json!({ "device": lease_or_sim }),
            };
            let v = exec(c, out, "simulator.release", p).await?;
            out.ok(&v, |v| format!("Released; deleted {}.", s(v, "udid")));
        }
        SimCmd::Leases => {
            let v = exec(c, out, "simulator.leases", json!({})).await?;
            out.ok(&v, |v| {
                let rows: Vec<Vec<String>> = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|l| {
                        vec![
                            s(l, "id"),
                            s(l, "udid"),
                            s(&l["holder"], "kind"),
                            l["expires_at"].as_str().unwrap_or("-").to_string(),
                        ]
                    })
                    .collect();
                if rows.is_empty() {
                    "No simulator leases.".into()
                } else {
                    table(&["LEASE", "UDID", "HOLDER", "EXPIRES"], rows)
                }
            });
        }
        SimCmd::Clone { sim: d, name } => {
            let v = exec(
                c,
                out,
                "simulator.clone",
                json!({ "device": d, "name": name }),
            )
            .await?;
            out.ok(&v, |v| {
                format!("Created {} ({}).", s(v, "name"), s(v, "udid"))
            });
        }
        SimCmd::Rename { sim: d, name } => {
            let v = exec(
                c,
                out,
                "simulator.rename",
                json!({ "device": d, "name": name }),
            )
            .await?;
            out.ok(&v, |_| format!("Renamed to {name}."));
        }
        SimCmd::Boot { sim: d, force } => {
            let v = exec(
                c,
                out,
                "simulator.boot",
                json!({ "device": d, "force": force }),
            )
            .await?;
            out.ok(&v, |v| format!("{} is booted.", s(v, "name")));
        }
        SimCmd::Shutdown { sim: d } => {
            let v = exec(c, out, "simulator.shutdown", json!({ "device": d })).await?;
            out.ok(&v, |_| format!("{d} is shut down."));
        }
        SimCmd::Erase { sim: d, force } => {
            let v = exec(
                c,
                out,
                "simulator.erase",
                json!({ "device": d, "force": force }),
            )
            .await?;
            out.ok(&v, |_| format!("Erased {d}."));
        }
        SimCmd::Delete { sim: d, force } => {
            let v = exec(
                c,
                out,
                "simulator.delete",
                json!({ "device": d, "force": force }),
            )
            .await?;
            out.ok(&v, |_| format!("Deleted {d}."));
        }
        SimCmd::Open { sim: d } => {
            let v = c.call("simulator.details", json!({ "device": d })).await?;
            let (udid, name) = (s(&v, "udid"), s(&v, "name"));
            open_in_simulator(&udid).await?;
            out.ok(&json!({ "opened": udid }), |_| {
                format!("Opened {name} in Simulator.")
            });
        }
        SimCmd::Screenshot { sim: d, output } => {
            let mut p = json!({ "device": d });
            if let Some(o) = output {
                p["path"] = json!(abs(o));
            }
            let v = exec(c, out, "simulator.screenshot", p).await?;
            out.ok(&v, |v| format!("Saved {}.", s(v, "path")));
        }
        SimCmd::Record {
            sim: d,
            output,
            seconds,
        } => {
            let mut p = json!({ "device": d });
            if let Some(o) = output {
                p["path"] = json!(abs(o));
            }
            if let Some(sec) = seconds {
                p["max_seconds"] = json!(sec);
            }
            let v = c.call("simulator.record", p).await?;
            if !out.json && !out.quiet {
                eprintln!("Recording {d}; press Ctrl-C to stop and save.");
            }
            let v = tokio::select! {
                r = follow(c, out, v.clone(), None) => r?,
                _ = tokio::signal::ctrl_c() => {
                    c.call("simulator.record_stop", json!({ "device": d })).await?;
                    follow(c, out, v, None).await?
                }
            };
            out.ok(&v, |v| {
                format!("Saved {} ({} s).", s(v, "path"), v["seconds"])
            });
        }
        SimCmd::Install { sim: d, app } => {
            let app = abs(app);
            let v = exec(
                c,
                out,
                "simulator.install",
                json!({ "device": d, "app": app }),
            )
            .await?;
            out.ok(&v, |_| format!("Installed {}.", app.display()));
        }
        SimCmd::Uninstall { sim: d, bundle } => {
            let v = exec(
                c,
                out,
                "simulator.uninstall",
                json!({ "device": d, "bundle": bundle }),
            )
            .await?;
            out.ok(&v, |_| format!("Uninstalled {bundle}."));
        }
        SimCmd::Apps { sim: d } => {
            let v = exec(c, out, "simulator.apps", json!({ "device": d })).await?;
            out.ok(&v, |v| {
                let rows = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| {
                        vec![
                            s(a, "bundle_id"),
                            s(a, "name"),
                            s(a, "version"),
                            s(a, "kind"),
                        ]
                    })
                    .collect();
                table(&["BUNDLE", "NAME", "VERSION", "KIND"], rows)
            });
        }
        SimCmd::Launch {
            sim: d,
            bundle,
            relaunch,
            args,
        } => {
            let v = exec(
                c,
                out,
                "simulator.launch",
                json!({ "device": d, "bundle": bundle, "relaunch": relaunch, "args": args }),
            )
            .await?;
            out.ok(&v, |_| format!("Launched {bundle}."));
        }
        SimCmd::Terminate { sim: d, bundle } => {
            let v = exec(
                c,
                out,
                "simulator.terminate",
                json!({ "device": d, "bundle": bundle }),
            )
            .await?;
            out.ok(&v, |_| format!("Quit {bundle}."));
        }
        SimCmd::Push {
            sim: d,
            bundle,
            local,
            remote,
        } => {
            let v = exec(
                c,
                out,
                "simulator.push_file",
                json!({ "device": d, "bundle": bundle, "local": abs(local), "remote": remote }),
            )
            .await?;
            out.ok(&v, |v| format!("Copied to {}.", s(v, "remote")));
        }
        SimCmd::Pull {
            sim: d,
            bundle,
            remote,
            local,
        } => {
            let local = abs(local);
            let v = exec(
                c,
                out,
                "simulator.pull_file",
                json!({ "device": d, "bundle": bundle, "remote": remote, "local": local }),
            )
            .await?;
            out.ok(&v, |_| format!("Copied to {}.", local.display()));
        }
        SimCmd::Log {
            sim: d,
            seconds,
            lines,
            predicate,
        } => {
            let v = exec(
                c,
                out,
                "simulator.log",
                json!({ "device": d, "seconds": seconds, "lines": lines, "predicate": predicate }),
            )
            .await?;
            out.ok(&v, |v| s(v, "text"));
        }
        SimCmd::OpenUrl { sim: d, url } => {
            let v = exec(
                c,
                out,
                "simulator.open_url",
                json!({ "device": d, "url": url }),
            )
            .await?;
            out.ok(&v, |_| format!("Opened {url}."));
        }
        SimCmd::Location { sim: d, lat, lon } => {
            let v = exec(
                c,
                out,
                "simulator.location_set",
                json!({ "device": d, "lat": lat, "lon": lon }),
            )
            .await?;
            out.ok(&v, |_| format!("Location set to {lat}, {lon}."));
        }
        SimCmd::Route {
            sim: d,
            file,
            speed,
        } => {
            let v = exec(
                c,
                out,
                "simulator.location_route",
                json!({ "device": d, "file": abs(file), "speed_mps": speed }),
            )
            .await?;
            out.ok(&v, |v| {
                format!("Following a route of {} points.", v["points"])
            });
        }
        SimCmd::LocationClear { sim: d } => {
            let v = exec(c, out, "simulator.location_clear", json!({ "device": d })).await?;
            out.ok(&v, |_| "Location cleared.".into());
        }
        SimCmd::Appearance { sim: d, mode } => {
            let v = exec(
                c,
                out,
                "simulator.appearance",
                json!({ "device": d, "mode": mode }),
            )
            .await?;
            out.ok(&v, |v| s(v, "mode"));
        }
        SimCmd::StatusBar {
            sim: d,
            time,
            battery,
            network,
            clear,
        } => {
            let v = exec(
                c,
                out,
                "simulator.status_bar",
                json!({ "device": d, "time": time, "battery_level": battery, "data_network": network, "clear": clear }),
            )
            .await?;
            out.ok(&v, |_| String::new());
        }
        SimCmd::Privacy {
            sim: d,
            action,
            service,
            bundle,
        } => {
            let v = exec(
                c,
                out,
                "simulator.privacy",
                json!({ "device": d, "action": action, "service": service, "bundle": bundle }),
            )
            .await?;
            out.ok(&v, |_| String::new());
        }
        SimCmd::Notify {
            sim: d,
            bundle,
            payload,
        } => {
            let text = std::fs::read_to_string(&payload)
                .map_err(|e| JidrError::usage(format!("{}: {e}", payload.display())))?;
            let payload: Value = serde_json::from_str(&text)
                .map_err(|e| JidrError::usage(format!("{}: {e}", payload.display())))?;
            let v = exec(
                c,
                out,
                "simulator.push_notification",
                json!({ "device": d, "bundle": bundle, "payload": payload }),
            )
            .await?;
            out.ok(&v, |_| "Sent.".into());
        }
        SimCmd::Pasteboard { sim: d, text } => {
            let v = exec(
                c,
                out,
                "simulator.pasteboard",
                json!({ "device": d, "text": text }),
            )
            .await?;
            out.ok(&v, |v| s(v, "text"));
        }
        SimCmd::AddMedia { sim: d, files } => {
            let files: Vec<_> = files.into_iter().map(abs).collect();
            let v = exec(
                c,
                out,
                "simulator.add_media",
                json!({ "device": d, "files": files }),
            )
            .await?;
            out.ok(&v, |v| format!("Added {}.", v["added"]));
        }
        SimCmd::TrustCert { sim: d, cert } => {
            let v = exec(
                c,
                out,
                "simulator.trust_cert",
                json!({ "device": d, "cert": abs(cert) }),
            )
            .await?;
            out.ok(&v, |_| "Trusted.".into());
        }
        SimCmd::Tap { sim: d, x, y, ms } => {
            let v = exec(
                c,
                out,
                "simulator.tap",
                json!({ "device": d, "x": x, "y": y, "duration_ms": ms }),
            )
            .await?;
            out.ok(&v, |_| String::new());
        }
        SimCmd::Swipe {
            sim: d,
            x1,
            y1,
            x2,
            y2,
            ms,
        } => {
            let v = exec(
                c,
                out,
                "simulator.swipe",
                json!({ "device": d, "x1": x1, "y1": y1, "x2": x2, "y2": y2, "duration_ms": ms }),
            )
            .await?;
            out.ok(&v, |_| String::new());
        }
        SimCmd::Text { sim: d, text } => {
            let v = exec(
                c,
                out,
                "simulator.text",
                json!({ "device": d, "text": text }),
            )
            .await?;
            out.ok(&v, |_| String::new());
        }
        SimCmd::Key { sim: d, key } => {
            let v = exec(c, out, "simulator.key", json!({ "device": d, "key": key })).await?;
            out.ok(&v, |_| String::new());
        }
        SimCmd::Rotate {
            sim: d,
            orientation,
        } => {
            let v = exec(
                c,
                out,
                "simulator.rotate",
                json!({ "device": d, "orientation": orientation }),
            )
            .await?;
            out.ok(&v, |v| s(v, "orientation"));
        }
        SimCmd::Tree { sim: d } => {
            let v = exec(c, out, "simulator.ui_tree", json!({ "device": d })).await?;
            out.ok(&v, |v| {
                let rows = v["elements"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|e| {
                        let f = &e["frame"];
                        let at = match (
                            f["x"].as_f64(),
                            f["y"].as_f64(),
                            f["width"].as_f64(),
                            f["height"].as_f64(),
                        ) {
                            (Some(x), Some(y), Some(w), Some(h)) => format!("{x},{y} {w}×{h}"),
                            _ => String::new(),
                        };
                        let depth = e["depth"].as_u64().unwrap_or(0) as usize;
                        vec![
                            format!("{}{}", "  ".repeat(depth), s(e, "type")),
                            s(e, "label"),
                            s(e, "identifier"),
                            at,
                        ]
                    })
                    .collect();
                table(&["TYPE", "LABEL", "ID", "FRAME (PX)"], rows)
            });
        }
    }
    Ok(())
}

/// The lease id `x` names, when it's a lease id rather than a simulator.
async fn uuid_lease(c: &Client, x: &str) -> Option<String> {
    let leases = c.call("simulator.leases", json!({})).await.ok()?;
    leases
        .as_array()?
        .iter()
        .find(|l| {
            l["id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(x))
        })
        .map(|l| s(l, "id"))
}

/// Shows the simulator in Xcode's Simulator app.
pub async fn open_in_simulator(udid: &str) -> JidrResult<()> {
    let status = tokio::process::Command::new("/usr/bin/open")
        .args(["-a", "Simulator", "--args", "-CurrentDeviceUDID", udid])
        .status()
        .await
        .map_err(|e| JidrError::sdk(format!("cannot run open: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(JidrError::sdk(
            "couldn't open Simulator; is Xcode installed?",
        ))
    }
}
