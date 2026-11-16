// Copyright 2026 Jidr contributors
// SPDX-License-Identifier: Apache-2.0

import XCTest

/// An error reply: `{"id":n,"error":{"message":…}}`.
struct CommandError: Error {
    let message: String
    init(_ message: String) { self.message = message }
}

/// The protocol's methods, each a thin wrapper over public XCUI API.
/// Coordinates and frames are in points.
final class Commands {
    static let version = "0.1.0"
    static let springboardID = "com.apple.springboard"

    private let springboard = XCUIApplication(bundleIdentifier: Commands.springboardID)
    private var cachedScale: Double?

    func handle(_ method: String, _ params: [String: Any]) throws -> Any {
        switch method {
        case "ping":
            return ["version": Commands.version, "os": UIDevice.current.systemVersion]
        case "screen":
            return screen()
        case "tap":
            return try tap(params)
        case "swipe":
            return try swipe(params)
        case "text":
            return try text(params)
        case "key":
            return try key(params)
        case "rotate":
            return try rotate(params)
        case "tree":
            return try tree(params)
        case "stop":
            return ["stopping": true]
        default:
            throw CommandError("unknown method `\(method)`")
        }
    }

    // MARK: - Screen

    private func screen() -> [String: Any] {
        let frame = springboard.frame
        return [
            "width": Double(frame.width),
            "height": Double(frame.height),
            "scale": scale(frame),
            "orientation": Commands.name(of: XCUIDevice.shared.orientation),
        ]
    }

    /// Pixels per point, read once from a screenshot (orientation-independent).
    private func scale(_ frame: CGRect) -> Double {
        if let cachedScale { return cachedScale }
        let image = XCUIScreen.main.screenshot().image
        let pixels = image.cgImage.map { Double(max($0.width, $0.height)) } ?? 0
        let points = Double(max(frame.width, frame.height))
        let value = points > 0 && pixels > 0 ? (pixels / points).rounded() : 0
        if value > 0 { cachedScale = value }
        return value
    }

    // MARK: - Touch

    /// A screen point, as an offset from SpringBoard's top-left corner.
    private func point(_ x: Double, _ y: Double) -> XCUICoordinate {
        springboard.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(dx: x, dy: y))
    }

    private func tap(_ p: [String: Any]) throws -> Any {
        let target = point(try number(p, "x"), try number(p, "y"))
        if let ms = optionalNumber(p, "duration_ms"), ms > 0 {
            target.press(forDuration: ms / 1000)
        } else {
            target.tap()
        }
        return [:] as [String: Any]
    }

    private func swipe(_ p: [String: Any]) throws -> Any {
        let x1 = try number(p, "x1"), y1 = try number(p, "y1")
        let x2 = try number(p, "x2"), y2 = try number(p, "y2")
        let seconds = max(optionalNumber(p, "duration_ms") ?? 300, 10) / 1000
        let distance = hypot(x2 - x1, y2 - y1)
        let velocity = XCUIGestureVelocity(CGFloat(max(distance / seconds, 1)))
        point(x1, y1).press(
            forDuration: 0.05, thenDragTo: point(x2, y2),
            withVelocity: velocity, thenHoldForDuration: 0)
        return [:] as [String: Any]
    }

    // MARK: - Keyboard and buttons

    private func text(_ p: [String: Any]) throws -> Any {
        guard let text = p["text"] as? String else { throw CommandError("missing `text`") }
        let (id, app) = foregroundApp(p)
        app.typeText(text)
        return ["bundle_id": id]
    }

    private func key(_ p: [String: Any]) throws -> Any {
        guard let name = p["name"] as? String else { throw CommandError("missing `name`") }
        switch name {
        case "home":
            XCUIDevice.shared.press(.home)
        case "volume_up", "volume_down":
            #if targetEnvironment(simulator)
                throw CommandError("`\(name)` is not available in the Simulator (XCUIDevice)")
            #else
                XCUIDevice.shared.press(name == "volume_up" ? .volumeUp : .volumeDown)
            #endif
        default:
            throw CommandError("unknown key `\(name)` (home, volume_up, volume_down)")
        }
        return [:] as [String: Any]
    }

    private func rotate(_ p: [String: Any]) throws -> Any {
        let orientations: [String: UIDeviceOrientation] = [
            "portrait": .portrait,
            "portrait_upside_down": .portraitUpsideDown,
            "landscape_left": .landscapeLeft,
            "landscape_right": .landscapeRight,
        ]
        guard let name = p["orientation"] as? String, let value = orientations[name] else {
            throw CommandError(
                "`orientation` must be portrait, landscape_left, landscape_right or portrait_upside_down")
        }
        XCUIDevice.shared.orientation = value
        return screen()
    }

    // MARK: - UI tree

    private func tree(_ p: [String: Any]) throws -> Any {
        let (id, app) = foregroundApp(p)
        let root = try app.snapshot()
        var elements: [[String: Any]] = []
        Commands.flatten(root, depth: 0, into: &elements)
        return ["bundle_id": id, "elements": elements]
    }

    private static func flatten(
        _ node: XCUIElementSnapshot, depth: Int, into out: inout [[String: Any]]
    ) {
        let f = node.frame
        out.append([
            "type": ElementTypes.name(node.elementType),
            "identifier": node.identifier,
            "label": node.label,
            "value": json(node.value),
            "frame": [
                "x": finite(f.origin.x), "y": finite(f.origin.y),
                "width": finite(f.width), "height": finite(f.height),
            ],
            "enabled": node.isEnabled,
            "selected": node.isSelected,
            "depth": depth,
        ])
        for child in node.children {
            flatten(child, depth: depth + 1, into: &out)
        }
    }

    // MARK: - Helpers

    /// The app in front: the first of `bundle_ids` running in the foreground,
    /// else SpringBoard (the home screen, Spotlight, system alerts).
    private func foregroundApp(_ p: [String: Any]) -> (String, XCUIApplication) {
        for id in p["bundle_ids"] as? [String] ?? [] where id != Commands.springboardID {
            let app = XCUIApplication(bundleIdentifier: id)
            if app.state == .runningForeground { return (id, app) }
        }
        return (Commands.springboardID, springboard)
    }

    private func number(_ p: [String: Any], _ key: String) throws -> Double {
        guard let value = optionalNumber(p, key) else {
            throw CommandError("missing number `\(key)`")
        }
        return value
    }

    private func optionalNumber(_ p: [String: Any], _ key: String) -> Double? {
        guard let value = (p[key] as? NSNumber)?.doubleValue, value.isFinite else { return nil }
        return value
    }

    /// JSON can't carry NaN or infinity (off-screen frames can have them).
    private static func finite(_ v: CGFloat) -> Any {
        v.isFinite ? Double(v) : NSNull()
    }

    private static func json(_ value: Any?) -> Any {
        switch value {
        case nil: return NSNull()
        case let s as String: return s
        case let n as NSNumber: return n.doubleValue.isFinite ? n : NSNull()
        case let v?: return String(describing: v)
        }
    }

    private static func name(of orientation: UIDeviceOrientation) -> String {
        switch orientation {
        case .portrait: return "portrait"
        case .portraitUpsideDown: return "portrait_upside_down"
        case .landscapeLeft: return "landscape_left"
        case .landscapeRight: return "landscape_right"
        case .faceUp: return "face_up"
        case .faceDown: return "face_down"
        default: return "unknown"
        }
    }
}
