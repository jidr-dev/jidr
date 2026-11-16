# Parity with Genymotion Desktop

Hand-edited. A row is ✅ only with evidence: a test name or a `file:line`. Report two numbers together, never breadth alone:

- **Breadth**: ✅ rows / all rows.
- **Ready for real work**: a judged estimate, with reasons.

Status: ✅ done with evidence · 🚧 partly done · ⬜ not started · ⏭️ post-MVP (linked issue) · ❌ won't do · 🔎 needs research (D11)

Tests named `*_emulator` run on a real emulator: on Linux KVM in CI (API 30, 33, 35) and on Apple Silicon in `cargo xtask e2e-mac`. The others run offline against `TestDaemon` (fake backend and SDK) or fakes.

| Feature | Genymotion tier | Jidr status | Evidence |
|---|---|---|---|
| Create / edit / clone / delete device | Free | ✅ | `crates/jidr-sdk/tests/avd_emulator.rs:48` `create_clone_boot_delete`; `crates/jidr-daemon/tests/conformance.rs:85` `device_lifecycle` (create, edit, clone, delete) |
| Factory reset | Free | ✅ | `conformance.rs:85` `device_lifecycle` (`device.reset`); `apps/jidr/tests/cli.rs:9` `device_lifecycle_json` (`device reset`) |
| Start / stop, cold boot | Free | ✅ | `crates/jidr-backend/tests/launch_emulator.rs:16` `boot_battery_stop_ramdisk_quickboot` (boot, stop, cold boot); `scripts/e2e-cli.sh` (start, stop) |
| Quick boot | Paid | ✅ | `launch_emulator.rs:16` `boot_battery_stop_ramdisk_quickboot`; `conformance.rs:288` `settings_drive_create_and_quick_boot`. Resume takes about 13.5 s on the dev Mac against a 5 s target |
| Device list and details | Free | ✅ | `conformance.rs:85` `device_lifecycle` (`device.list`, `device.details`); `cli.rs:96` `discovery_commands` |
| Log zip | Free | ✅ | `apps/jidr/tests/maintenance.rs:49` `device_log_bundle` (`jidr device logs --zip`) |
| CLI automation (`gmtool admin/device`) | Paid | ✅ | `cli.rs:9` `device_lifecycle_json`, `cli.rs:44` `exit_codes_are_error_codes`; `scripts/e2e-cli.sh` (create → start → install → logcat → stop → delete, JSON throughout) |
| Install APK / push / pull / logcat | Free | ✅ | `scripts/e2e-cli.sh` (install, logcat); `crates/jidr-adb/tests/fake_adb.rs:139` `push_pull`; `crates/jidr-daemon/tests/jobs_events.rs:112` `logcat_follow_streams_lines` |
| Zip flash | Free | ✅ | A dropped zip and `jidr flash` both call `file.flash` (ADR-0004): Magisk and Zygisk modules install on Full-root devices, recovery, Riru and image zips are refused with the reason, anything else goes to `/sdcard/Download`. `crates/jidr-root/tests/module_emulator.rs` `flashed_module_loads_then_is_removed` (installed, loaded, `service.sh` and `system.prop` live, removed after a reboot); `crates/jidr-root/tests/flash_mode.rs` (routing); `crates/jidr-root/src/flash.rs` (detection). Image flashing isn't possible on the emulator (no fastboot; see ADR-0004) |
| Root (Android ≤ 11) | Free | ✅ | `crates/jidr-root/tests/adb_emulator.rs:47` `adb_root_cycle_none_adb_none` (API 30; API 31 in `e2e-mac`) |
| Root toggle (Android 12+) | Paid | ✅ | `adb_emulator.rs:47` `adb_root_cycle_none_adb_none` (API 33 and 35 in CI); `crates/jidr-root/tests/adb_mode.rs` (live switch, cold-boot apply, verify) |
| Magisk root | — (Jidr only) | ✅ | `crates/jidr-root/tests/magisk_emulator.rs:49` `magisk_root_cycle_none_magisk_none`; `scripts/e2e-root.sh` (none → adb → magisk → none, `su -c id`) |
| Full root on Play Store images (guided beta) | — (Jidr only) | 🚧 | Guided grant (ADR-0002): `crates/jidr-root/src/magisk/grant.rs:124` `grant_shell_guided`, unit-tested offline; not run end to end on a Play Store image. Unattended options: ⏭️ [#24](https://github.com/jidr-dev/jidr/issues/24) |
| Battery widget | Free | ✅ | `crates/jidr-widgets/tests/widgets_emulator.rs` `widgets_core_commands` (`dumpsys battery`) |
| GPS widget | Free | ✅ | `crates/jidr-widgets/tests/gps_emulator.rs` `gps_fix_and_route_replay` (`dumpsys location`) |
| GPS routes (GPX/KML) | Paid | ✅ | `gps_emulator.rs` `gps_fix_and_route_replay` (3-point GPX in order) |
| Rotation | Free | ✅ | `widgets_core_commands` (`sensor.rotate`, `dumpsys window` rotation); player touches in all four rotations: `crates/jidr-ui/tests/player_emulator.rs` `taps_land_under_the_pointer_in_every_rotation` |
| Motion sensors 3D | Paid | 🚧 | `sensor.set` / `sensor.get` (`crates/jidr-widgets/src/sensors.rs`); rotation and the light sensor are tested in `widgets_core_commands`, accelerometer and gyroscope presets aren't |
| Phone call / SMS | Free | ✅ | `widgets_core_commands` (`mCallState` ringing and idle, SMS in the inbox) |
| Baseband gsm signal/data/voice | Paid | ✅ | `widgets_core_commands` (signal level 0 and ≥ 3 on any radio via `phone.gsm`); data and voice registration via `phone.gsm` |
| Baseband operator & SIM identity | Paid | ⏭️ | [#16](https://github.com/jidr-dev/jidr/issues/16); the phone number alone works natively |
| Network Wi-Fi / mobile toggles, speed/delay profiles | Paid | ✅ | `widgets_core_commands` (Wi-Fi via `settings`, mobile data via the emulated radio and `network.get`, speed via `adb emu network status`) |
| Network packet loss / bad DNS | Paid | ✅ | `crates/jidr-root/tests/helper_emulator.rs` `helper_starts_as_root_and_drops_packets` (netem loss, slow and failing DNS lookups, both back after a cold boot; API 33 and 35 in CI, `unsupported` on API 30); offline `crates/jidr-widgets/tests/shaping.rs` `shaping_is_reapplied_after_a_cold_boot`. Needs Shell or Full root and API 31+ ([#17](https://github.com/jidr-dev/jidr/issues/17)) |
| Identifiers (Android ID, IMEI) | Free | 🚧 | Android ID: `identity.set` gives every installed app the new ID (its SSAID) and sets the global value, through the on-device helper: `crates/jidr-widgets/tests/identity.rs` offline, `crates/jidr-root/tests/identity_emulator.rs` `android_id_reaches_apps_and_survives_a_cold_boot` (an app reads it, before and after a cold boot) (not run on CI yet). IMEI: shown by `identity.get`, but it can't be changed without modifying the image: the modem's IMEI is a constant and SELinux leaves no way to relay the radio's modem port (tried on API 35, see [root.md](root.md#identifiers-android-id-and-imei-19)), [#19](https://github.com/jidr-dev/jidr/issues/19) stays open for it |
| Build props | Free | ✅ | `crates/jidr-root/tests/props_emulator.rs:61` `props_spoof_after_cold_boot_and_stock_after_revert` (Full-root devices only) |
| Disk I/O limit | Free | ⏭️ | [#20](https://github.com/jidr-dev/jidr/issues/20) |
| Camera / media injection | Paid | 🚧 | Host webcam as a camera at the next start: `widgets_core_commands` (`camera.set` → `-camera-back webcam0` in the launch args). Image and video files: ⏭️ [#21](https://github.com/jidr-dev/jidr/issues/21) |
| Microphone / audio injection | Paid | 🚧 | `audio.inject` / `audio.stop` stream a WAV or raw PCM file over gRPC `injectAudio` ([#70](https://github.com/jidr-dev/jidr/issues/70)): WAV parsing in `crates/jidr-backend/src/audio/wav.rs` (unit tests), packet chunking, pacing and stop in `crates/jidr-backend/src/audio/mod.rs` (unit tests), the commands against the fake backend in `crates/jidr-widgets/tests/audio.rs` `inject_checks_its_input_and_reports_an_unreachable_emulator`. `crates/jidr-widgets/tests/audio_emulator.rs` `inject_busy_and_stop` hasn't run yet, and nothing checks what an app hears. No player widget; the Mac's microphone isn't routed |
| Biometrics | Paid | 🚧 | Fingerprint touch and remove: `widgets_core_commands` (the fingerprint HAL is checked on HIDL and AIDL images). Dirty / partial / insufficient / too fast: ⏭️ [#22](https://github.com/jidr-dev/jidr/issues/22) |
| Android controls (D-pad, volume, media keys) | Paid | ✅ | `widgets_core_commands` (Home → launcher focus, volume panel) |
| Dev tools (show taps, pointer location, layout bounds, animations) | Paid | ✅ | `widgets_core_commands` (`dev.set`, read back with `settings get` and `getprop debug.layout`) |
| Gamepad | Paid | 🚧 | `input.gamepad.*` ([#23](https://github.com/jidr-dev/jidr/issues/23)) replays a controller on this Mac into the device as a USB Xbox 360 controller through AOSP's `hid` command, no root: the HID descriptor, reports and forwarding loop in `crates/jidr-widgets/src/gamepad/` (unit tests), the commands against the fake backend in `crates/jidr-widgets/tests/gamepad.rs`. The guest side was checked by hand on API 30 and 35 (`dumpsys input`: `GAMEPAD \| JOYSTICK`, `Vendor_045e_Product_028e.kl`, `BUTTON_A` and joystick events); not yet run with a physical controller. macOS hosts only |
| Device Link | Free | ❌ | Rejected (D37) |
| Clipboard sharing | Free | 🚧 | `clipboard.get` / `clipboard.set` tested in `widgets_core_commands`; the two-way sharing toggle in the player (`crates/jidr-ui/src/player/session.rs`) isn't |
| Drag-drop APK / files | Free | ✅ | `crates/jidr-ui/src/player/drop.rs:79` `drops_route_by_type_and_root`; manual check (a dropped APK installed and opened) |
| Screenshot | Free | ✅ | `crates/jidr-widgets/tests/media_emulator.rs` `media_screenshot_and_five_minute_recording` |
| Screen recording | Free | ✅ | `media_screenshot_and_five_minute_recording` (ffprobe: h264, 300 s, 9000 frames). Audio: ⏭️ [#28](https://github.com/jidr-dev/jidr/issues/28) |
| Snapshot manager | — (Jidr only) | ✅ | `crates/jidr-widgets/tests/snapshot_emulator.rs` `snapshot_round_trip_and_cross_root_refusal` |
| Shared folders | — (Jidr only) | ⏭️ | [#27](https://github.com/jidr-dev/jidr/issues/27) |
| Multitouch via modifiers | Free | 🚧 | ⌥ pinch and ⇧⌥ pan: `crates/jidr-ui/src/player/input.rs` `gestures_add_a_second_finger` (unit test only) |
| Pixel-perfect, fullscreen, resize | Free | ✅ | Pixel-perfect and resize (golden `player-pixel-perfect`); fullscreen ⌃⌘F, Esc leaves (goldens `player-fullscreen`, `player-fullscreen-reveal`; `crates/jidr-ui/src/player/fullscreen.rs` unit tests; native window: manual check on a Mac) |
| MCP server for AI agents | — (Jidr only) | ✅ | `crates/jidr-mcp/tests/mcp.rs:25` `tools_are_listed_with_annotations`, `mcp.rs:163` `destructive_policy_d5`; `crates/jidr-mcp/tests/agent_tasks.rs:175` `five_agent_tasks` (real emulator) |
| USB phones next to the emulators | — (Jidr only) | 🚧 | `physical.*` over plain adb ([#92](https://github.com/jidr-dev/jidr/issues/92)): list with live plug/unplug events, readiness hints, screenshot, recording, input, install, push/pull, logcat, shell and reboot against a fake adb in `crates/jidr-daemon/tests/physical.rs`; parsing in `crates/jidr-adb/src/physical.rs` unit tests; the Phones group's goldens `phones-empty`, `phones-ready`, `phones-unauthorized`. `crates/jidr-daemon/tests/physical_device.rs` (`JIDR_TEST_PHONE_SERIAL`) hasn't run on a real phone yet. No root, snapshots or sensor widgets on phones |
| Shell REPL (Genymotion Shell) | Free | ✅ | `jidr shell` ([#73](https://github.com/jidr-dev/jidr/issues/73)): `apps/jidr/tests/shell.rs` `piped_lines_run_with_the_current_device`, `a_script_stops_at_its_first_failure`, `source_runs_a_file_inside_the_shell`; device context and completion: `apps/jidr/src/shell/line.rs` `fills_in_the_current_device`, `apps/jidr/src/shell/complete.rs` `completes_commands_devices_and_methods` |

**Breadth:** 30 / 44 ✅ (68%). Of the other 14: 9 🚧, 4 ⏭️, 1 ❌. Six rows are Jidr-only (not in Genymotion); three of them are ✅.

**Ready for real work: about 70% for the core use case, lower for Genymotion power users.**

- **Core use case:** create a device on Apple Silicon, root it, test an app, and automate it from a CLI, CI or an AI agent. Every step has an emulator test, and the whole path passes `cargo xtask e2e-mac` (345 emulator tests plus the CLI, root and SDK-untouched scripts) and CI on API 30, 33 and 35.
- **What holds it back from "ready":**
  - There's no signed release yet; v0.1.0 is in preparation.
  - Quick boot is slower than its target.
  - A few emulator tests are still flaky on shared CI runners.
  - Several paid-tier niches are partial or post-MVP: motion presets, file-based camera, microphone injection (not yet run on an emulator), biometric scenarios, network loss and DNS, identifiers (the IMEI can't be changed), disk limits, gamepad (no emulator test yet).
  - Clipboard sharing in the player isn't finished.

## iOS simulators (Jidr only, D50)

Not a Genymotion feature. Runs on macOS with full Xcode; the CI smoke test needs a macOS runner (not added yet).

| Feature | Jidr status | Evidence |
|---|---|---|
| List, create, clone, boot, shut down, erase, delete | 🚧 | `crates/jidr-simctl/src/parse.rs` tests (simctl JSON); `crates/jidr-daemon/src/simulator.rs` tests (picking, ownership guard, events). Checked by hand against Xcode 27.1 (list, runtimes, device types, details). |
| One simulator per agent session (`simulator.lease`) | 🚧 | `crates/jidr-daemon/src/simulator.rs` `leases_of_a_closed_connection_expire_after_grace`; not yet run end to end on a simulator |
| Screenshot, recording | 🚧 | Screenshot checked by hand through `jidr sim screenshot` (1320×2868, Xcode 27.1); recording not yet |
| Apps, files, logs, URLs, location and routes, appearance, status bar, permissions, push notifications, pasteboard, media, CA trust | 🚧 | Wired to public `simctl` subcommands; not yet run on a simulator |
| Tap, swipe, text, keys, rotation, UI tree | 🚧 | `crates/jidr-daemon/src/sim_runner.rs` tests (key mapping, pixel frames); the XCTest runner (`helpers/jidr-sim-runner`) builds with `cargo xtask sim-runner` but hasn't run on a simulator yet |
| MCP: `simulator_lease`, device tools on a UDID | 🚧 | `crates/jidr-mcp/tests/mcp.rs` `tools_are_listed_with_annotations`; `crates/jidr-mcp/src/lib.rs` tests |
| Desktop app: Simulators group, preview, Open in Simulator | 🚧 | `crates/jidr-ui/src/simulators.rs`; not yet in the goldens |
| Snapshots, root, sensors, network shaping | ❌ | No simulator API (`docs/research/ios-simulators.md`) |
