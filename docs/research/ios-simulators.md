# Research: iOS Simulators in Jidr

- **Status:** decided as D50 (2026-11-16). The maintainer chose phases 1 and 2 plus input through an XCTest runner (phase 3) in one go, with simulators in Xcode's own set and one lease per agent session. The XCTest-runner spike's numbers go in `helpers/jidr-sim-runner/README.md`.
- **Date:** 2026-11-16
- **Question:** can Jidr show and drive iOS Simulators on macOS next to its Android emulators and USB phones (D46), "the way simslim does", and how would that fit the daemon, CLI, MCP and the desktop app?

## Summary

- **simslim doesn't stream screens or send input.** It is a memory tool: it turns off the launchd daemons a simulator doesn't need, so one Mac can run about 4x as many simulators. The screens and input in its demo video come from MobAI, a separate commercial app whose device layer is closed source. So simslim is a model for one part of the job (many simulators on one Mac, which is Jidr's D42), not for the player.
- **Most of what Jidr does for a phone can be done for a simulator with public tools.** `xcrun simctl` covers create/boot/shutdown/erase/clone/delete, install/launch, screenshots, screen recording, deep links, location and routes, appearance, status bar, permissions, push notifications, pasteboard, photos and root certificates. I checked all of these in `simctl help` with Xcode 27.1 on this Mac.
- **Touch, keys, text, rotation and a UI tree have no public command-line API.** The two ways to get them are an XCTest runner (public API, slower) or Apple's private SimulatorKit framework (fast, breaks with Xcode releases). That choice is the main risk and needs a spike before it's settled, as ADR-0001 was for the Android player.
- **Fit:** a separate `simulator.*` command family keyed by UDID, like `physical.*` (D46), not a second `Backend`. CoreSimulator owns simulator state the way adb owns phones.
- **Recommendation: a smaller slice first.** Ship the `simctl` part (daemon, CLI, MCP screenshots, a view-only screen in the app with "Open in Simulator"), then decide on input after a spike. Don't build a 60 fps player on private APIs unless users ask for it: Simulator.app already gives every user who has Xcode a good one.

## 1. How simslim works

Repository: <https://github.com/MobAI-App/simslim> (MIT, copyright Interlap; Go CLI plus a SwiftUI app). I read the README, `CLAUDE.md`, `docs/` and the source at commit `7e374b3`.

**What it does.** A freshly booted iOS simulator starts about 180 background services (Siri, Spotlight, photo analysis, iCloud, widgets…). simslim disables an allowlist of them, grouped into 16 categories (`siri`, `search`, `icloud`, `store`, …). Its README's numbers (M1 Pro, 16 GB, iOS 26.5): 258 processes and 4.0 GB `phys_footprint` stock against 70 processes and 0.9 GB slim. Its demo runs 12 slim simulators on a 16 GB laptop; stock ones "start thrashing at around 5".

**How it does it.**

- Everything goes through `xcrun simctl` (`list -j`, `boot`, `shutdown`, `clone`, `erase`, `delete`, `spawn`). `simctl.go` is the only file that runs it.
- Slimming is `launchctl disable <label>` run *inside* the simulator with `simctl spawn <udid> launchctl …`, eight at a time, then a reboot. The overrides persist across reboots on iOS/tvOS 18.5, watchOS 11.5 and visionOS 2.5 or later. On older runtimes (`--no-reboot`) it also runs `launchctl bootout`, and the state is lost at the next boot.
- A faster "offline" path writes the overrides straight into the store `launchd_sim` reads at boot. That file isn't inside the device's data directory: it's `/private/var/tmp/com.apple.CoreSimulator.SimDevice.<UDID>/disabled.plist`, next to the device's `launchd.log`. simslim's own notes call it "an undocumented CoreSimulator detail", always read the result back with `launchctl print-disabled`, and fall back to `launchctl` when it doesn't work. Their measurement: 2m14s down to 33s for 170 labels.
- `watch` follows the device-set directories with kqueue and slims each new simulator from its `device.plist` before its first boot. This also catches the throwaway clones `xcodebuild` makes for parallel testing.
- `measure` sums `phys_footprint` over the processes under the simulator's `launchd_sim` (`pgrep`, `ps`, `top`), which is the figure that decides how many fit.
- `clone` copies a simulator's apps, data and slim profile, rebases symlinks and checks with `lsof` that the clone has nothing open in the source.

**What it needs.** macOS with full Xcode and a simulator runtime: `simctl` isn't in the Command Line Tools, and simslim's error message says so (`xcode-select -s /Applications/Xcode.app`). No entitlements, no root, no private frameworks loaded into its own process. The undocumented `disabled.plist` path is its only use of CoreSimulator internals.

**Screen and input.** None. MobAI (<https://mobai.run>) does that part. Its public repositories (`mobai-ci`, `mobai-mcp`, `mobile-harness`) are clients or binary installers for the MobAI desktop app, so how MobAI streams and drives simulators isn't public, and I didn't look further.

**Can Jidr reuse it?** Yes. MIT is on the clean-room allowed list ("permissively licensed code", like avdroot), so its ideas and code can be ported with attribution in `NOTICE`. The parts worth reusing are the category allowlist of launchd labels, the `launchctl` method and the read-back, and the measuring method. Each would need a row in the clean-room log. Simplest of all, Jidr could call an installed `simslim` instead of porting it, the way it calls `adb`. Nothing in simslim stops Jidr from using its ideas.

## 2. What macOS gives us (checked on this Mac, read-only)

Environment: Xcode 27.1 (27A9275), runtimes iOS 18.5, 26.1 and 27.1, 131 device types. Two simulators were already booted by someone else; I didn't boot, create or delete any. The only thing I ran against a booted one was `simctl io … screenshot`, `simctl io … enumerate` and `simctl ui … appearance` (reads).

**`simctl` subcommands** (Xcode 27.1): `addmedia`, `appinfo`, `boot`, `clone`, `create`, `delete`, `diagnose`, `erase`, `get_app_container`, `getenv`, `icloud_sync`, `install`, `install_app_data`, `io`, `keychain`, `launch`, `list`, `listapps`, `location`, `logverbose`, `openurl`, `pair`, `pair_activate`, `pbcopy`, `pbpaste`, `pbsync`, `personalization`, `privacy`, `push`, `reboot`, `rename`, `runtime`, `shutdown`, `spawn`, `status_bar`, `terminate`, `ui`, `uninstall`, `unpair`, `upgrade`. `bootstatus <udid> [-b]` (wait for a boot to finish, booting first with `-b`) works too, though the list leaves it out. There is **no tap, swipe, key, text, rotate, shake or UI-tree command**.

Findings worth keeping:

- **Screenshots:** `simctl io <udid> screenshot --type=png <file>` took 0.6 s per frame (1.0 s for the first) on a 1320×2868 iPhone 17 Pro Max with the host load average at about 9. That's about 1.6 frames a second, the same order as the `adb exec-out screencap` loop the app uses for USB phones (D46, 1–3 fps). JPEG was a little faster (about 0.5 s).
- **`-` is not stdout in Xcode 27.1.** `simctl help io` says `screenshot … (use "-" for stdout)`, but it wrote a file named `-` in the current directory. Jidr should write to a temp file. This is the kind of churn the risk section is about.
- **`simctl io enumerate`** lists the framebuffer as an IOSurface (`BGRA`, 1320×2868, 5312 bytes per row). That's what idb's fast stream reads, through private API.
- **`recordVideo`** writes a QuickTime movie (`--codec h264|hevc`), prints `Recording started` on stderr, and finalises on SIGINT. That maps to `media.record` / `record_stop` as they are.
- **`location`** has `set`, `clear`, named scenarios and `start --speed … <lat,lon> <lat,lon>…`, so GPX/KML routes (D14) can be replayed by sending the waypoints.
- **`ui`** has `appearance light|dark`, `increase_contrast` and `content_size` (Dynamic Type).
- **`status_bar override`** sets time, network type, Wi-Fi and cell bars, carrier name, battery state and level (display only).
- **`keychain add-root-cert`** trusts a CA, which is what HTTPS interception needs (on Android that needs root).
- **Runtimes:** `simctl runtime list -v` reports 7.3 GB (iOS 27.1), 7.8 GB (26.1) and 8.2 GB (18.5). `xcodebuild -downloadPlatform iOS [-buildVersion …] [-architectureVariant universal|arm64]` downloads one; `-importPlatform` adds a downloaded `.dmg`.
- **Xcode itself:** the two installed copies are 3.6 GB and 4.6 GB on disk, without runtimes. So a first-time user needs roughly 4 GB of Xcode plus about 8 GB per iOS runtime.
- **SimulatorKit** is at `Xcode.app/Contents/SharedFrameworks/SimulatorKit.framework` and **CoreSimulator** at `/Library/Developer/PrivateFrameworks/CoreSimulator.framework`. I only listed them; neither is documented.
- Nothing from idb, AXe or Maestro is installed here.

## 3. How it fits Jidr

### A second `Backend`, or a separate family?

**A separate `simulator.*` family, like `physical.*` (D46).** Reasons:

- The `Backend` trait is shaped around the AOSP emulator: `LaunchSpec` takes an AVD, `sysdir`, the `emulator` and `adb` binaries; `InstanceInfo` carries console, adb and gRPC ports and a gRPC token; `save_snapshot`, `stop_without_save`, `wait_powered_off` and `has_clients` are emulator ideas. A simulator has none of these. Faking them would make every `Backend` caller lie.
- Jidr's `Device` model carries root modes, image refs, hardware profiles, network shaping, snapshots and pools. Few of them apply, which is the same reason D46 kept phones out of `Device`.
- Jidr doesn't own the process. `CoreSimulatorService` boots and keeps simulators, and they live on in `~/Library/Developer/CoreSimulator/Devices` whether Jidr runs or not. That's the adb situation: Jidr asks, the system owns.
- The daemon stays the owner of commands, jobs, events and locks, so the "every action is a protocol command" rule (AGENTS.md) holds.

**Shape.**

- A new Apache-2.0 crate, `jidr-simctl`, wrapping `xcrun simctl` with typed results, built on the `-j` JSON output wherever it exists. It compiles on every platform and returns a typed error off macOS (as `media.record` does on Linux, D49). It gets a fake `simctl` for offline tests, like `jidr-adb`'s fake adb.
- A `simulator` provider in `jidr-daemon` (or its own crate) registering `simulator.*` through `CommandProvider`. These are new commands, so the protocol change is additive.
- Simulators are named by **UDID**. `simulator.list` also returns the name, runtime, platform (iOS, tvOS, watchOS, visionOS), state and device type.
- **Device sets.** By default Jidr works on Xcode's own set (what users already have, and what Xcode's Run button targets). Agent and pool simulators go into Jidr's own set (`simctl --set ~/Library/Application Support/Jidr/simulators`), so agents never touch a user's simulators. This mirrors D3 and the agent-owned rule in D5. Deleting or erasing a simulator Jidr didn't create needs an explicit flag from the CLI and `--allow-destructive` from MCP.
- **Events:** poll `simctl list devices -j` every few seconds while a client listens, or watch the device-set directories with kqueue the way simslim does. Either way, `simulator.list` gets live state changes like `physical.list` does.
- **RAM budget (D42):** a stock simulator is about 4 GB `phys_footprint`, more than a lean emulator, so booting one should count against the host budget, measured the way simslim measures.

Proposed commands (names to settle in the issue):

| Command | `simctl` underneath |
|---|---|
| `simulator.list`, `simulator.details`, `simulator.runtimes`, `simulator.device_types` | `list devices/runtimes/devicetypes -j`, `runtime list -j` |
| `simulator.create`, `.clone`, `.rename`, `.erase`, `.delete` | same names |
| `simulator.boot`, `.shutdown`, `.reboot` (long-running jobs) | `boot` then `bootstatus -b`, `shutdown`, `reboot` |
| `simulator.screenshot` | `io screenshot` to a temp file |
| `simulator.record`, `.record_stop` | `io recordVideo`, then SIGINT |
| `simulator.install`, `.uninstall`, `.apps`, `.launch`, `.terminate` | `install` (a simulator `.app`, or a zip of one), `uninstall`, `listapps`, `launch`, `terminate` |
| `simulator.push_file`, `.pull_file` | copy into or out of `get_app_container <bundle> data` on the host file system |
| `simulator.open_url` | `openurl` |
| `simulator.location.set`, `.route`, `.clear` | `location set`, `location start` (waypoints from Jidr's GPX/KML parser), `location clear` |
| `simulator.appearance`, `.content_size`, `.status_bar` | `ui`, `status_bar` |
| `simulator.privacy`, `.push_notification`, `.pasteboard`, `.add_media`, `.trust_cert` | `privacy`, `push`, `pbcopy`/`pbpaste`, `addmedia`, `keychain add-root-cert` |
| `simulator.log` (follow job) | `spawn <udid> log stream --style ndjson` |
| `simulator.runtime.download` (job) | `xcodebuild -downloadPlatform …` |
| `simulator.tap`, `.swipe`, `.key`, `.text`, `.rotate`, `.ui_tree` | not `simctl`: see "Input" below |

`simulator.shell` is left out on purpose: iOS runtimes have no shell (simslim found only `df` and `launchctl` in the runtime's `bin`), and `simctl spawn` of arbitrary host paths is broader than Jidr should expose.

**CLI:** `jidr sim list|boot|shutdown|screenshot|install|open-url|location|…`, with `--json` like the rest.

**MCP:** the generic tools already accept "a device name or id, or a USB phone's adb serial". Add "or a simulator UDID", routed by a `DeviceIo::simulator` next to `DeviceIo::phone`. `screenshot` works in phase 1; `tap`, `swipe`, `type_text`, `press_key` and `ui_dump` answer a typed "not available for simulators yet" error until input lands. `logcat_tail` maps to the unified log.

### Screen and input in the desktop app

The player's ADR-0001 path (gRPC frames into a Slint `SharedPixelBuffer`) doesn't exist for simulators. Options:

| Path | Frames | Input | API status | Cost |
|---|---|---|---|---|
| **A. `simctl io screenshot` loop** | about 1–2 fps (measured 0.6 s per frame) | none | public | S. Same code shape as the phone screen (`crates/jidr-ui/src/phones.rs`) |
| **B. XCTest runner** that Jidr ships (Swift, Apache-2.0), started per simulator with `xcodebuild test-without-building` and reached over a local socket | screenshots through XCTest, a few fps | `XCUICoordinate` taps and drags, `typeText`, `XCUIDevice` home and volume buttons and orientation; UI tree from `XCUIElement.snapshot()` | public, documented API | L. Like ADR-0003's helper. Each runner is an extra process on the simulator and takes several seconds to start; latency to be measured. This is how WebDriverAgent and Maestro drive iOS |
| **C. SimulatorKit (private)** in a separate helper process: read the framebuffer IOSurface, send HID events | 30–60 fps | touches, buttons, keyboard: low latency | **private**, no docs. Prior art: idb's FBSimulatorControl (MIT) and AXe (MIT) | L, plus upkeep every Xcode release |
| **D. ScreenCaptureKit on Simulator.app's window, input with `CGEvent`** | 60 fps | through the window | public, but needs Screen Recording and Accessibility permissions and a visible Simulator.app window | M. Fragile (window layout, device bezels, Spaces) and confusing for users |
| **E. "Open in Simulator"** | Simulator.app | Simulator.app | public (`open -a Simulator --args -CurrentDeviceUDID <udid>`) | S |

Recommended order: **A + E** first (a view-only preview in the launcher with a button that opens the real thing), then **B** if the spike says it's fast enough for agents. Only consider **C**, behind a setting and with automatic fallback to A/B, if users want Jidr's own player for simulators.

### What users need to install

- **Full Xcode** from the Mac App Store or developer.apple.com (an Apple Account is needed for either), about 4 GB on disk here. The Command Line Tools aren't enough. Jidr should detect `xcode-select -p` pointing at the CLT and give simslim's kind of hint.
- **Xcode's licence and first-launch setup:** `sudo xcodebuild -license accept` and `xcodebuild -runFirstLaunch`. Jidr must never accept Apple's licence for the user. That's the same rule as D8 and AGENTS.md's "never accept a Google SDK license on a user's behalf"; Jidr only says what to run.
- **A simulator runtime:** about 7–8 GB each, from Xcode's settings or `xcodebuild -downloadPlatform iOS`. Jidr can run that as a job with progress. Still to check: whether it needs a signed-in Apple Account on the command line.
- **Signing and entitlements:** none for path A/E. `simctl` is a subprocess, and Jidr.app's Developer ID signature and hardened runtime don't matter for it. Path B: the runner is built for the simulator, which only needs ad-hoc signing ("Sign to Run Locally"), so no Apple team is needed. It can be built in CI on a macOS runner and shipped inside Jidr.app and the `.mcpb`. Path C: keep the private frameworks in a separate helper process. Loading them into a hardened-runtime app may need `com.apple.security.cs.disable-library-validation`; that needs checking, and the helper keeps it out of the main app either way. Path D: Screen Recording and Accessibility (TCC) prompts.
- **Licensing for Jidr:** nothing of Apple's is bundled or redistributed, so the D18 split is unaffected. The `jidr-simctl` crate and the runner are Apache-2.0. Apple's Xcode licence limits simulators to Apple-branded hardware, which is already the only place they run.

## 4. Feature parity

✅ public tool does it · 🟡 partly, or with a caveat · 🔧 needs the input path (B or C) · ❌ not possible or doesn't apply

| Jidr feature (Android) | iOS Simulator | How | Phase |
|---|---|---|---|
| List devices, live state | ✅ | `simctl list -j` + polling or kqueue | 1 |
| Create / clone / rename / delete | ✅ | `create`, `clone`, `rename`, `delete` | 1 |
| Factory reset | ✅ | `erase` | 1 |
| Start / stop / reboot | ✅ | `boot` + `bootstatus`, `shutdown`, `reboot` | 1 |
| Quick boot / named snapshots / hibernate | ❌ | no snapshot API. `clone` of a shut-down simulator is the nearest thing (fast APFS copy) | — |
| Hardware profiles | 🟡 | fixed Apple device types (131 here), no custom RAM, CPU or screen | 1 |
| System images, Play images | 🟡 / ❌ | runtimes via `xcodebuild -downloadPlatform`; there's no App Store in the simulator | 1 |
| Screenshot | ✅ | `io screenshot` | 1 |
| Screen recording | ✅ | `io recordVideo` (QuickTime, H.264 or HEVC) | 1 |
| Install / uninstall / launch / stop app | ✅ | simulator `.app` builds only, not App Store `.ipa`s | 1 |
| Push / pull files | ✅ | host paths from `get_app_container`; `addmedia` for Photos | 1 |
| Logs (logcat) | ✅ | `log stream --style ndjson` inside the simulator | 1 |
| Deep links | ✅ | `openurl` | 1 |
| GPS fix, GPX/KML routes | ✅ | `location set`, `location start` with waypoints | 1 |
| Dark mode, font scale | ✅ | `ui appearance`, `ui content_size` | 1 |
| Battery, signal | 🟡 | `status_bar override` changes only what's drawn | 1 |
| Permissions without prompts | ✅ | `privacy grant` (no Android equivalent in Jidr today) | 1 |
| Push notifications | ✅ | `push` (no Android equivalent) | 1 |
| Clipboard | ✅ | `pbcopy` / `pbpaste` | 1 |
| Trust a proxy CA | ✅ | `keychain add-root-cert`, no root needed | 1 |
| Tap / swipe / type / keys | 🔧 | XCTest runner (B) or SimulatorKit HID (C) | 3 |
| Rotation | 🔧 | `XCUIDevice.orientation` (B), or Simulator.app | 3 |
| UI tree (`ui_dump`) | 🔧 | `XCUIElement.snapshot()` (B), or the private accessibility bridge idb uses | 3 |
| Face ID / Touch ID match | 🟡 | community-documented `notifyutil` notifications to `BiometricKit_Sim`; to verify | 3 |
| Shake | ❌ | Simulator.app menu only | — |
| Root, Magisk, build props, zip flash, modules | ❌ | doesn't apply: simulator apps run as host processes and their containers are host folders | — |
| Sensors (accelerometer, proximity, light…), camera, audio injection, game controller | ❌ | no simulator API | — |
| Network speed, delay, loss | ❌ | Network Link Conditioner is host-wide, not per simulator | — |
| Identifiers (Android ID, IMEI) | ❌ | — | — |
| RAM budget, many devices, agent pools (D42) | 🟡 | `clone` into Jidr's own set for leases; slimming à la simslim | 5 |
| MCP | 🟡 | `screenshot` and `command_run` in 1, input tools in 3 | 1, 3 |
| Linux build | ❌ | macOS only; typed error elsewhere | — |

About half of the Android widget panel doesn't apply. The useful iOS set is smaller and different: permissions, push notifications, status bar and the CA trust have no Jidr counterpart today.

## 5. Phases

| Phase | Scope | Size |
|---|---|---|
| 0. Decision and spike | A `D<n>` row and an ADR draft. Spike: (a) an XCTest runner's start time, tap latency and memory on one simulator; (b) whether `-downloadPlatform` works without a signed-in account; (c) whether `recordVideo` can write to a pipe. | S |
| 1. `simulator.*` over `simctl` | `jidr-simctl` crate with a fake `simctl`; the provider; list, lifecycle, screenshot, record, apps, files, log, open URL, location and routes, appearance, status bar, privacy, push, pasteboard, media, CA trust, runtimes and download job; Xcode and CLT detection with hints; `jidr sim …`; MCP routing for screenshots; `docs/cli.md`, `docs/mcp.md`, the protocol changelog, a parity section and a `skills/` update; CI on a GitHub macOS runner, which has Xcode and runtimes and runs simulators natively (no nested virtualization) | M |
| 2. Desktop app | A "Simulators" section in the launcher, the screenshot preview loop (path A), "Open in Simulator", toolbar actions (appearance, location, open URL, drop a `.app` to install, record); all strings through `@tr` / `tr!` and translated (D47) | M |
| 3. Input for agents and the app | The XCTest runner (path B): Swift source in the repo, built in CI, shipped in Jidr.app and the `.mcpb`; started on demand and stopped when idle; `simulator.tap/swipe/key/text/rotate/ui_tree`; MCP `tap`, `swipe`, `type_text`, `press_key`, `ui_dump` routed to it; the app's preview takes input | L |
| 4. Fast player (optional) | Path C in a separate helper, behind a setting, with fallback to A/B; its own ADR; checked against each new Xcode | L |
| 5. Many simulators (optional) | Leases of clones from Jidr's own set for agents (D42 for iOS); slimming through an installed `simslim`, or a port of its allowlist with attribution | M |

## 6. Risks

- **Private APIs.** SimulatorKit and the CoreSimulator internals (path C, simslim's `disabled.plist`) have no docs or stability promise. idb, AXe and simslim all carry fixes per Xcode version. Mitigation: keep phases 1–3 on public API and isolate anything private in a helper with a fallback.
- **Xcode churn.** `simctl` changes between releases. Example from this Mac: `screenshot -` writes a file named `-` in Xcode 27.1 although the help still says stdout. Parse `-j` JSON, not text; pin the behaviour in tests against a fake `simctl`; run a smoke job on GitHub's macOS runner image, which follows new Xcode releases. An XCTest runner built with one Xcode may need rebuilding for the next major one.
- **Size and setup.** About 4 GB of Xcode plus about 8 GB per runtime, an Apple Account and a licence prompt. It's a lot for an Android user who just wants to try it, though iOS developers already have it. The UI should only show the Simulators section once Xcode is found.
- **Memory.** Stock simulators are heavy (about 4 GB each per simslim's numbers), so a few booted simulators and emulators together can push the Mac into swap. Count simulators in the D42 budget.
- **Licensing.** Jidr can't ship Xcode, runtimes or Simulator.app, and can't accept Apple's licence for the user. Apple's licence keeps simulators on Apple hardware. Ported MIT code (simslim, idb, AXe) needs `NOTICE` entries. Appium's WebDriverAgent shows as `NOASSERTION` on GitHub; read its licence file before porting anything from it. Writing the runner from Apple's XCTest docs avoids that question.
- **Clean-room rules.** `docs/clean-room.md` is written for Android and Genymotion. It would need iOS rows: allowed sources are Apple's public docs (XCTest, `simctl help`, `xcodebuild -help`), behaviour observed by running Apple's tools, and MIT/Apache code (simslim, idb, AXe, Maestro, XcodeBuildMCP). Using an undocumented path such as simslim's `disabled.plist` gets a log row. Reverse-engineering Apple binaries for path C (class-dumping SimulatorKit) is not the kind of source that file allows; using it only through idb's MIT code keeps provenance clear, but that's a call for the maintainer. Add "no Apple logos" next to the Google, Android and Genymotion rule.
- **Scope and identity.** Jidr is "a Genymotion Desktop alternative". iOS adds a second platform to test, document and translate, with no Genymotion parity behind it. Several free tools already cover parts of it: Simulator.app itself, XcodeBuildMCP (MIT) for agents, idb (MIT), Maestro (Apache-2.0). Jidr's selling point would be one daemon, CLI, MCP and app for Android emulators, Android phones and iOS simulators together.

## 7. Recommendation

**Do a smaller slice.** Phases 0 and 1, and phase 2 without input: the `simulator.*` family over public `simctl`, the CLI, MCP screenshots and the useful iOS-only commands (permissions, push notifications, CA trust), plus a view-only preview and "Open in Simulator" in the app. It reuses the D46 pattern almost one for one, runs entirely on public tools, needs no signing or entitlements, and can be tested on GitHub's macOS runners.

Then decide phase 3 from the spike numbers. If an XCTest runner taps in a few hundred milliseconds and starts in under about 15 seconds, it's good enough for agents and keeps Jidr off private APIs. Leave phase 4 (a 60 fps player on SimulatorKit) out unless users ask for it, since Simulator.app already exists and the upkeep is ongoing. Phase 5 can follow whenever agent pools on iOS are wanted, ideally by calling `simslim` rather than copying it.

Don't start any of it before the maintainer adds a `D<n>` row: it changes the product's scope.

## 8. Draft GitHub issue

> Not posted. For the maintainer to edit and open.

---

**Title:** iOS Simulators: a `simulator.*` command family over `simctl` (macOS)

**Labels:** `upcoming feature`

### What

Show and control the Mac's iOS Simulators from Jidr, next to Android emulators and USB phones: the daemon, `jidr sim …`, MCP and the desktop app. Research: `docs/research/ios-simulators.md`.

### Why

Teams that build for Android and iOS, and agents that test both, could use one tool and one MCP server for both. Most of what Jidr does for a USB phone (D46) has a public `simctl` equivalent, and some useful iOS commands don't exist on the Android side yet (permissions, push notifications, trusting a proxy CA).

### Scope of the first slice

- A new Apache-2.0 crate `jidr-simctl` (typed `xcrun simctl` wrapper, `-j` JSON, fake `simctl` for tests; typed error off macOS).
- `simulator.*` commands keyed by UDID, additive to the protocol: list (with live state), details, runtimes, device types, create, clone, rename, erase, delete, boot, shutdown, reboot, screenshot, record / record_stop, install / uninstall / apps / launch / terminate, push_file / pull_file, log (follow), open_url, location set / route / clear, appearance, content_size, status_bar, privacy, push_notification, pasteboard, add_media, trust_cert, runtime.download (job).
- Xcode's default device set for the user's simulators; Jidr's own set for anything agents create. Erase and delete of a simulator Jidr didn't create need an explicit flag (`--allow-destructive` from MCP, D5).
- Booted simulators count against the RAM budget (D42).
- CLI `jidr sim …` with `--json`. MCP: `screenshot` accepts a simulator UDID; the input tools return a typed "not available for simulators yet".
- Desktop app: a Simulators section shown when Xcode is found, a screenshot-loop preview like the phone screen, "Open in Simulator", toolbar actions, all strings translated.
- Detect a missing Xcode or a CLT-only `xcode-select` and say what to run. Never accept Apple's licence for the user.
- Docs: `docs/cli.md`, `docs/mcp.md`, the protocol changelog, `docs/parity.md`, `skills/`, and iOS rows in `docs/clean-room.md`.
- CI: a smoke job on a GitHub macOS runner (simulators run natively there).

### Not in this slice

- Touch, swipe, keys, text, rotation and the UI tree. A spike compares an XCTest runner (public API) with SimulatorKit (private) first.
- A 60 fps player; snapshots and hibernate (no API); root, sensors, network shaping (don't apply).
- Linux and Windows.

### Open questions

1. Command prefix: `simulator.*` (covers tvOS, watchOS and visionOS) or `ios.*`?
2. Should Jidr create user-facing simulators in Xcode's set, so Xcode's Run button sees them, or only in its own set?
3. Port simslim's slimming (MIT) later, or only call an installed `simslim`?
4. Does `xcodebuild -downloadPlatform` need a signed-in Apple Account on the command line?

---
