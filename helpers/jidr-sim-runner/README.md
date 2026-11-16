# jidr-sim-runner

Input for iOS Simulators. `xcrun simctl` can boot, install, launch, screenshot and record, but it can't tap, swipe, type, rotate or read the UI tree. This runner does those with Apple's public XCTest UI-automation API (`XCUIApplication`, `XCUICoordinate`, `XCUIDevice`, `XCUIElementSnapshot`), and nothing private.

It is a UI-test bundle with no app under test. Its one test method, `testServe`, serves line-delimited JSON on `127.0.0.1:<port>` until a client sends `stop`. Simulator processes share the host's network stack, so the host connects to the same loopback address.

Licence: Apache-2.0. Written from Apple's XCTest documentation and headers and from observed behaviour (see [docs/clean-room.md](../../docs/clean-room.md)); no WebDriverAgent code.

## Build

It's built on the user's Mac on first use, not shipped, so there's no signed binary to notarize (simulator builds are signed ad hoc).

```bash
cargo xtask sim-runner                 # into target/jidr-sim-runner, prints the .xctestrun path
cargo xtask sim-runner --out <dir>     # into <dir> (an xcodebuild derived-data dir)
```

which runs:

```bash
xcodebuild build-for-testing -quiet \
  -project helpers/jidr-sim-runner/JidrSimRunner.xcodeproj -scheme JidrSimRunner \
  -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath <dir> CODE_SIGN_IDENTITY=-
```

and leaves `<dir>/Build/Products/JidrSimRunner_iphonesimulator<sdk>-arm64-x86_64.xctestrun`. The Xcode project is hand-written (no XcodeGen or Tuist): one `bundle.ui-testing` target, four Swift files, a shared scheme whose test action uses the Release configuration. Deployment target iOS 17.0 (the XCTest in Xcode 27 is built for 17).

## Run

```bash
TEST_RUNNER_JIDR_PORT=<port> TEST_RUNNER_JIDR_TOKEN=<token> \
  xcodebuild test-without-building -xctestrun <path>.xctestrun -destination id=<udid>
```

xcodebuild strips the `TEST_RUNNER_` prefix, so the runner sees `JIDR_PORT` and `JIDR_TOKEN`. Without both, the test fails at once. The simulator must be booted. xcodebuild installs `JidrSimRunner-Runner.app` on it, launches it, and the test binds the port; the host polls `connect` + `ping` until it answers. `stop` ends the test and xcodebuild exits 0. If the host dies without sending `stop`, the runner keeps serving until the simulator shuts down or the runner app is terminated (`xcrun simctl terminate <udid> dev.jidr.simrunner.xctrunner`).

## Protocol

One JSON object per line, both ways. Requests `{"id":n,"method":"…","params":{…}}`; replies `{"id":n,"result":…}` or `{"id":n,"error":{"message":"…"}}`. `params` may be omitted.

**Auth.** The first line on a connection must carry the token as a top-level `"token"` field (`{"id":1,"token":"…","method":"ping"}`); it's then handled like any request. A missing or wrong token gets `{"id":…,"error":{"message":"unauthorized"}}` and the connection is closed. Later lines need no token. Lines over 1 MiB close the connection.

Requests run one at a time on the main thread, in arrival order, across all connections. An XCTest failure during a request (e.g. typing with nothing focused) becomes that request's error and doesn't end the server.

All coordinates and frames are in points, origin top-left.

| Method | Params | Result |
|---|---|---|
| `ping` | | `{"version":"0.1.0","os":"<iOS version>"}` |
| `screen` | | `{"width","height","scale","orientation"}` (SpringBoard's frame; orientation is `portrait`, `portrait_upside_down`, `landscape_left`, `landscape_right`, `face_up`, `face_down` or `unknown`) |
| `tap` | `x`, `y`, `duration_ms?` | `{}`; with `duration_ms` it's a long press |
| `swipe` | `x1`, `y1`, `x2`, `y2`, `duration_ms?` (300) | `{}`; a 50 ms press, then a drag at distance/duration points per second |
| `text` | `text`, `bundle_ids?` | `{"bundle_id"}` of the app typed into |
| `key` | `name`: `home`, `volume_up`, `volume_down` | `{}`; volume buttons always error in the Simulator (XCTest marks them unavailable there) |
| `rotate` | `orientation`: `portrait`, `landscape_left`, `landscape_right`, `portrait_upside_down` | the `screen` result after rotating |
| `tree` | `bundle_ids?` | `{"bundle_id","elements":[…]}` |
| `stop` | | `{"stopping":true}`, then the test ends |

**Foreground app** (`text`, `tree`): XCTest can't list running apps, so the client passes candidate `bundle_ids` (e.g. from `simctl listapps`, or the app it just launched). The runner picks the first whose `state` is `runningForeground`, else SpringBoard (`com.apple.springboard`: home screen, Spotlight, system alerts).

**Touch**: `tap` and `swipe` are offsets from SpringBoard's top-left corner via `XCUICoordinate`, so they work whatever app is in front.

**Tree elements** are the foreground app's `snapshot()` walked depth first, one entry per node: `type` (the `XCUIElement.ElementType` case name: `button`, `staticText`, `searchField`, …), `identifier`, `label`, `value` (string, number or null), `frame` `{x,y,width,height}` (null for non-finite values), `enabled`, `selected`, `depth` (0 for the application).

## Spike results

Not measured yet: the spike needs a Mac with no other simulator or emulator running. To do: time from `xcodebuild test-without-building` to the first `ping`, `tap` round-trip latency, whether `text` finds the app in front, and how long `tree` takes on a busy screen.
