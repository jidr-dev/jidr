# Jidr for AI agents (MCP)

`jidr mcp` serves the [Model Context Protocol](https://modelcontextprotocol.io) on stdio. Agents get every Jidr command plus fast tools for driving a device: look, tap, type, read the UI. It starts the Jidr daemon if it isn't running.

## Add Jidr to Claude

Claude Code:

```bash
claude mcp add jidr -- jidr mcp
```

On Linux (headless, [cli.md](cli.md#linux-headless)) the command is the same once `jidr` is on your `PATH`; if Claude Code doesn't see it, give the full path: `claude mcp add jidr -- ~/.local/bin/jidr mcp`. The tools are the same as on the Mac, except screen recording of a device.

Claude Desktop (macOS): download `jidr-mcp-<version>-darwin-arm64.mcpb` from the [latest release](https://github.com/jidr-dev/jidr/releases/latest) and open it, or add Jidr by hand to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "jidr": { "command": "/opt/homebrew/bin/jidr", "args": ["mcp"] }
  }
}
```

Cursor (`~/.cursor/mcp.json`) takes the same `mcpServers` entry. `jidr uninstall` removes these entries again (D31).

Each release also publishes Jidr to the [MCP Registry](https://registry.modelcontextprotocol.io) as `io.github.jidr-dev/jidr`, pointing at that `.mcpb`.

## Tools

| Tool | Hints | What it does |
|---|---|---|
| `command_list {prefix?}` | read-only | Every Jidr command with its params schema and flags |
| `command_run {command, params, timeout_s?}` | may be destructive | Runs any command; long-running ones (boot, image install) are waited for, up to `timeout_s` (900 s), then returned as a job |
| `pool_get {base, ttl_s?, timeout_s?}` | interacts | A throwaway device of your own: a copy-on-write clone of a base, resumed from its snapshot in seconds. Returns `{lease, device, serial, connect}`. Without `ttl_s` the lease lasts as long as this MCP session, plus 2 minutes |
| `pool_release {lease \| device}` | destructive (your clone only) | Gives the clone back: it is stopped and deleted |
| `simulator_lease {device_type?, runtime?, ttl_s?}` | interacts | A fresh iOS simulator of your own (macOS with Xcode), created and booted, with input ready. Returns `{lease, udid, simulator, input}`. Without `ttl_s` it lasts as long as this MCP session, plus 2 minutes |
| `simulator_release {lease \| device}` | destructive (your simulator only) | Gives the simulator back: it is shut down and deleted |
| `jobs_list {all?}`, `jobs_cancel {job}` | read-only / idempotent | Long-running operations |
| `screenshot {device}` | read-only | PNG of the screen fitted in 1024×1024, plus the screen size and scale |
| `tap {device, x, y, coords?}` | interacts | Tap; coordinates are in the screenshot image unless `coords: "device"` |
| `swipe {device, x1, y1, x2, y2, duration_ms?, coords?}` | interacts | Swipe or drag (scroll, open the app drawer) |
| `type_text {device, text, enter?}` | interacts | Type printable ASCII into the focused field |
| `press_key {device, key}` | interacts | `GoHome`, `GoBack`, `AppSwitch`, `Enter`, `Power`, volume, arrows… |
| `ui_dump {device}` | read-only | uiautomator XML: text, ids and bounds to find what to tap |
| `logcat_tail {device, lines?, filter?}` | read-only | Recent log lines (the last N lines that match the filter) |
| `ca_cert_install {device, path}` | may restart the device | Trust a CA certificate (PEM or DER, on this Mac) as a system cert on a running Full-root device, then verify; `root.cacert.list` / `.verify` / `.remove` go through `command_run` |
| `command_run root.writable_system {device, enabled}` | restarts a running device | A writable `/system` on an `adb`-root device, or back to read-only (docs/root.md); refused with `usage` on other root modes, Play Store images and images older than API 31 |
| `proxy_set {device, host?, port}`, `proxy_clear {device}` | idempotent | The device's global HTTP proxy, for Burp or mitmproxy on the Mac (`host` defaults to `10.0.2.2`, the Mac as the emulator sees it) |

**USB phones (#92):** `screenshot`, `tap`, `swipe`, `type_text`, `press_key`, `ui_dump` and `logcat_tail` also take the adb serial of a phone plugged into this Mac with USB debugging on (`command_run physical.list` lists them). They go through adb only: the screenshot is `screencap` fitted in 1024×1024, keys are `input keyevent`, and the UI dump stays in `/data/local/tmp`. The other `physical.*` commands (install, push, pull, record, reboot) go through `command_run`; `physical.shell` is destructive and a phone is never agent-owned, so it needs `jidr mcp --allow-destructive`. There's no leasing or locking for phones: an agent should use a phone only when the user names it.

**Files:** `file.push`, `file.pull` and `file.install` go through `command_run`, as do synced folders (#27): `command_run {command: "file.sync.start", params: {device, local, remote?}}` keeps a folder on this Mac copied to the device (one way: a first copy, then each change, deletions included; `.git`, `node_modules` and `.DS_Store` are skipped, and a folder with more than 10000 files is refused unless `max_files` raises it). It returns the sync with its `id` and file count while the first copy runs; `file.sync.list` shows its state, `pushed` count and `last_error`, and `file.sync.stop {id}` ends it. A sync ends by itself when the device stops. Sync only folders the user named: deleting a file there deletes it on the device.

Everything else goes through `command_run`, e.g. playing a WAV on this Mac as the device's microphone for a voice flow: `command_run {command: "audio.inject", params: {device, path, loop?}, timeout_s: 5}` returns the job while it plays, and `command_run {command: "audio.stop", params: {device}}` ends it. A game controller on this Mac can drive a device the same way: `command_run {command: "input.gamepad.connect", params: {device}, timeout_s: 5}` returns the job, and `command_run {command: "input.gamepad.disconnect", params: {device}}` ends it. To record the screen with the device's audio: `command_run {command: "media.record", params: {device, max_seconds, audio?}}` (`audio` defaults to `true`; pass `false` for a silent video). The result's `audio` says whether the MP4 has an audio track; it's `false` when the device has no audio or its stream failed, and the recording still succeeds.

Every tool carries `readOnlyHint`, `destructiveHint`, `idempotentHint` and `openWorldHint` annotations. Jidr errors come back as tool results with `isError: true` and structured `{code, message, data}` (codes as in docs/control-protocol.md), so an agent can recover, e.g. start a stopped device after `not_found`.

**iOS simulators (D50):** `screenshot`, `tap`, `swipe`, `type_text`, `press_key`, `ui_dump` and `logcat_tail` also take a booted simulator's UDID. Coordinates work as on Android (the screenshot image unless `coords: "device"`, which means full-size screenshot pixels). `type_text` types any text; `press_key` knows `GoHome`, `Enter`, `Backspace`, `Tab` and the volume keys (there's no Back key on iOS: tap the app's back button). `ui_dump` returns the XCTest element tree of the app in front as JSON, with frames in screenshot pixels. The first input on a simulator builds and starts Jidr's XCTest runner (a minute or two the first time; `simulator_lease` does it for you). Everything else (install a build, launch, open a deep link, location, appearance, permissions, push notifications) goes through `command_run simulator.*`.

## Devices for agents (D42)

Use a device of your own, never the user's (for example not `emulator-5554`):

- **iOS:** `simulator_lease` gives each session its own simulator; use its `udid` for everything (`command_run simulator.install {device, app}`, `simulator.launch {device, bundle}`, then screenshot → tap → ui_dump) and call `simulator_release` when done. Never drive simulators you didn't lease; the user's are theirs.
- **Throwaway:** `pool_get {base}` (bases: `command_run base.list`; a person makes one with `jidr base create <device> <name>`). Use the returned `serial` with adb, Flutter or Gradle, and call `pool_release` when done. If the session ends without it, the daemon deletes the clone 2 minutes later. For long tasks pass `ttl_s` and extend it with `command_run pool.renew`.
- **Persistent:** `command_run device.create {name, image, profile: "agent-lean"}` (about 4 GB of host RAM instead of ~7.6 GB) and `device.start`; hibernate it with `device.hibernate` when idle.
- **Full machine:** `pool_get` waits up to `timeout_s` (120 s) for room in the host RAM budget, then fails with `resource_exhausted`. `device.start` fails with it at once; its `data.devices` says what uses the RAM and `data.hibernate_candidates` which running devices are idle longest. Hibernate or release your own devices, or wait; don't hibernate the user's.

## Safety policy (D5)

By default an agent can read, interact, start and stop. Commands flagged destructive (`device.delete`, `device.reset`, …) are refused unless:

- the device is **agent-owned**: devices an agent creates through `command_run device.create` are agent-owned automatically, and a person can mark one with `jidr device edit <name> --agent-owned true`; or
- the server runs as `jidr mcp --allow-destructive`.

Refusals carry `data.policy = "D5"`. Connection-level protocol methods (`hello`, `events.subscribe`, `logcat.follow`, `daemon.shutdown`) aren't callable through MCP.

## Measured latency (not gated)

From the five scripted agent tasks (`cargo nextest run -p jidr-mcp --features emulator`, M1 Pro, API 31 arm64): screenshot ~0.4–1.1 s, tap ~0.2–0.4 s, type_text ~0.7–1.2 s, press_key ~0.01–0.1 s, swipe 1–4 s, logcat_tail ~0.2–0.5 s, ui_dump 4–6 s median (uiautomator itself is slow), cold boot via `command_run device.start` 25–32 s.
