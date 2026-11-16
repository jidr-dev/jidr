// Copyright 2026 Jidr contributors
// SPDX-License-Identifier: Apache-2.0

import XCTest

/// The runner's only test. It serves Jidr's input protocol on
/// `127.0.0.1:$JIDR_PORT` until a client sends `stop`.
///
/// Every XCUI call runs here, on the main thread, one request at a time; the
/// network side only parses lines and queues them (see `Server`).
final class JidrSimRunnerTests: XCTestCase {
    /// Issues XCTest records while a request runs. They become that request's
    /// error instead of failing the test, so one bad tap never ends the server.
    private var captured: [String]?

    override func record(_ issue: XCTIssue) {
        if captured != nil {
            captured?.append(issue.compactDescription)
        } else {
            super.record(issue)
        }
    }

    func testServe() throws {
        continueAfterFailure = true
        let env = ProcessInfo.processInfo.environment
        guard let port = env["JIDR_PORT"].flatMap(UInt16.init), port != 0 else {
            XCTFail("JIDR_PORT must be a TCP port (pass TEST_RUNNER_JIDR_PORT to xcodebuild)")
            return
        }
        guard let token = env["JIDR_TOKEN"], !token.isEmpty else {
            XCTFail("JIDR_TOKEN must be set (pass TEST_RUNNER_JIDR_TOKEN to xcodebuild)")
            return
        }

        let server = try Server(port: port, token: token)
        try server.start()
        print("jidr-sim-runner: listening on 127.0.0.1:\(port)")

        let commands = Commands()
        while true {
            guard let job = server.inbox.next(timeout: 1) else { continue }
            captured = []
            var result: Result<Any, Error>
            do {
                result = .success(try commands.handle(job.method, job.params))
            } catch {
                result = .failure(error)
            }
            if let issues = captured, !issues.isEmpty, case .success = result {
                result = .failure(CommandError(issues.joined(separator: "; ")))
            }
            captured = nil
            job.reply(result)
            if job.method == "stop" {
                server.stop()
                print("jidr-sim-runner: stopped")
                return
            }
        }
    }
}
