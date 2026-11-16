//! iOS simulators in the launcher (D50): selection, a view-only preview and
//! the actions (boot, shut down, open in Simulator, screenshot, delete).
//!
//! The preview is `simctl io screenshot` in a loop, about one or two frames
//! a second, read with `simctl` directly, as the phone screen reads adb
//! (`phones.rs`); every action is a `simulator.*` command. Input belongs to
//! Simulator.app ("Open in Simulator") and to agents (MCP).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use jidr_core::{SimState, Simulator};
use serde_json::json;
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, Weak};
use tokio::task::JoinHandle;

use crate::AppWindow;
use crate::app::{Ctx, report, toast};
use crate::i18n::tr;

/// Fastest the preview refreshes; a screenshot itself takes longer.
const MIN_FRAME: Duration = Duration::from_millis(400);

#[derive(Default)]
struct Preview {
    /// The simulator the frame loop follows.
    udid: Option<String>,
    task: Option<JoinHandle<()>>,
}

thread_local! {
    static PREVIEW: RefCell<Preview> = RefCell::default();
}

fn selected(ctx: &Rc<Ctx>) -> Option<Simulator> {
    let st = ctx.state.borrow();
    let udid = st.selected_sim.as_ref()?;
    st.data.sims.iter().find(|s| &s.udid == udid).cloned()
}

pub fn wire(ctx: &Rc<Ctx>) {
    let app = &ctx.app;

    let c = ctx.clone();
    app.on_select_sim(move |i| {
        let udid = {
            let st = c.state.borrow();
            usize::try_from(i)
                .ok()
                .and_then(|i| st.data.sims.get(i))
                .map(|s| s.udid.clone())
        };
        {
            let mut st = c.state.borrow_mut();
            if st.selected_sim != udid {
                st.sim_open = false;
            }
            st.selected_sim = udid;
            st.selected_phone = None;
            st.phone_open = false;
        }
        c.rerender();
        crate::phones::sync(&c);
        sync(&c);
    });

    let c = ctx.clone();
    app.on_sim_action(move |a| action(&c, a.as_str()));
}

/// Starts or stops the preview loop to match the view (UI thread).
pub fn sync(ctx: &Rc<Ctx>) {
    let want = {
        let st = ctx.state.borrow();
        selected(ctx)
            .filter(|s| st.sim_open && s.state == SimState::Booted)
            .map(|s| s.udid)
    };
    if PREVIEW.with(|p| p.borrow().udid == want) {
        return;
    }
    PREVIEW.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(t) = p.task.take() {
            t.abort();
        }
        p.udid = want.clone();
    });
    ctx.app.set_sim_has_frame(false);
    ctx.app.set_sim_rate("".into());
    let Some(udid) = want else { return };
    let weak = ctx.app.as_weak();
    let task = ctx.bridge.handle().spawn(frames(udid, weak));
    PREVIEW.with(|p| p.borrow_mut().task = Some(task));
}

/// The frame loop: screenshot, decode, show; until aborted.
async fn frames(udid: String, weak: Weak<AppWindow>) {
    let simctl = match jidr_simctl::Simctl::detect().await {
        Ok(s) => s.with_timeout(Duration::from_secs(15)),
        Err(e) => return show_rate(&weak, e.message),
    };
    let mut times: Vec<Instant> = Vec::new();
    loop {
        let start = Instant::now();
        match simctl
            .screenshot_png(&udid)
            .await
            .and_then(|png| crate::phones::decode(&png))
        {
            Ok((rgba, w, h)) => {
                times.push(Instant::now());
                times.retain(|t| t.elapsed() < Duration::from_secs(5));
                let fps =
                    times.len() as f64 / 5.0_f64.min(times[0].elapsed().as_secs_f64().max(0.5));
                let rate = tr!(
                    "Preview · {} frames/s · open it in Simulator to use it",
                    format!("{fps:.1}")
                );
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = weak.upgrade() else { return };
                    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
                    app.set_sim_frame(Image::from_rgba8(buf));
                    app.set_sim_frame_w(w as i32);
                    app.set_sim_frame_h(h as i32);
                    app.set_sim_has_frame(true);
                    app.set_sim_rate(rate.into());
                });
            }
            Err(e) => {
                show_rate(&weak, tr!("No screen: {}", e.message));
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        if let Some(rest) = MIN_FRAME.checked_sub(start.elapsed()) {
            tokio::time::sleep(rest).await;
        }
    }
}

fn show_rate(weak: &Weak<AppWindow>, text: String) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            app.set_sim_rate(text.into());
        }
    });
}

/// Shows the simulator in Xcode's Simulator app.
fn open_in_simulator(udid: &str) -> bool {
    std::process::Command::new("/usr/bin/open")
        .args(["-a", "Simulator", "--args", "-CurrentDeviceUDID", udid])
        .status()
        .is_ok_and(|s| s.success())
}

fn action(ctx: &Rc<Ctx>, a: &str) {
    let Some(sim) = selected(ctx) else { return };
    let udid = sim.udid.clone();
    let name = sim.name.clone();
    let w = ctx.app.as_weak();
    match a {
        "sim-open" | "sim-close" => {
            ctx.state.borrow_mut().sim_open = a == "sim-open" && sim.state == SimState::Booted;
            ctx.rerender();
            sync(ctx);
        }
        "simulator" => {
            if !open_in_simulator(&udid) {
                toast(&w, tr!("Couldn't open Simulator; is Xcode installed?"));
            }
        }
        "screenshot" => {
            let w2 = w.clone();
            ctx.bridge.call(
                "simulator.screenshot",
                json!({ "device": udid }),
                move |r| match r {
                    Ok(v) => toast(
                        &w2,
                        tr!("Saved {}", v["path"].as_str().unwrap_or("the screenshot")),
                    ),
                    Err(e) => toast(&w2, e.message),
                },
            );
        }
        "boot" => {
            toast(&w, tr!("Booting {}…", name.clone()));
            let w2 = w.clone();
            ctx.bridge.run(
                "simulator.boot",
                json!({ "device": udid }),
                |_| {},
                move |r| {
                    if let Err(e) = r {
                        toast(&w2, e.message);
                    }
                },
            );
        }
        "shutdown" => {
            ctx.bridge.call(
                "simulator.shutdown",
                json!({ "device": udid }),
                report(&w, tr!("Shut down {}", name.clone())),
            );
        }
        "delete" if sim.jidr_owned => {
            let c2 = ctx.clone();
            let body = if sim.lease.is_some() {
                tr!(
                    "An agent is using it: its lease ends and its session loses the simulator. Its apps and data are deleted."
                )
            } else {
                tr!("Its apps and data are deleted. This can't be undone.")
            };
            crate::launcher::confirm(
                ctx,
                &tr!("Delete {}?", name.clone()),
                &body,
                &tr!("Delete"),
                move || {
                    let w = c2.app.as_weak();
                    {
                        let mut st = c2.state.borrow_mut();
                        st.selected_sim = None;
                        st.sim_open = false;
                    }
                    c2.rerender();
                    sync(&c2);
                    c2.bridge.call(
                        "simulator.delete",
                        json!({ "device": udid.clone() }),
                        report(&w, tr!("Deleted {}", name.clone())),
                    );
                },
            );
        }
        _ => {}
    }
}
