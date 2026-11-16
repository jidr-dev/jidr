// Copyright 2026 Jidr contributors
// SPDX-License-Identifier: Apache-2.0

import Foundation
import Network

/// A request parsed off the wire, waiting for the main thread.
struct Job {
    let method: String
    let params: [String: Any]
    let reply: (Result<Any, Error>) -> Void
}

/// Hands jobs from the network queue to the main thread.
final class Inbox {
    private let lock = NSLock()
    private let ready = DispatchSemaphore(value: 0)
    private var jobs: [Job] = []

    func push(_ job: Job) {
        lock.lock()
        jobs.append(job)
        lock.unlock()
        ready.signal()
    }

    /// The next job, or nil after `timeout` seconds without one.
    func next(timeout: TimeInterval) -> Job? {
        guard ready.wait(timeout: .now() + timeout) == .success else { return nil }
        lock.lock()
        defer { lock.unlock() }
        return jobs.isEmpty ? nil : jobs.removeFirst()
    }
}

/// Line-delimited JSON over TCP on the loopback interface. Simulator
/// processes share the host's network stack, so the host connects to the
/// same `127.0.0.1:<port>`.
final class Server {
    let inbox = Inbox()
    private let listener: NWListener
    private let token: [UInt8]
    private let queue = DispatchQueue(label: "dev.jidr.simrunner.net")

    init(port: UInt16, token: String) throws {
        guard let nwPort = NWEndpoint.Port(rawValue: port) else {
            throw CommandError("invalid port \(port)")
        }
        let params = NWParameters.tcp
        params.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: nwPort)
        params.allowLocalEndpointReuse = true
        listener = try NWListener(using: params)
        self.token = Array(token.utf8)
    }

    /// Starts listening and waits until the socket is bound.
    func start() throws {
        let bound = DispatchSemaphore(value: 0)
        var failure: Error?
        listener.stateUpdateHandler = { state in
            switch state {
            case .ready:
                bound.signal()
            case .failed(let error):
                failure = error
                bound.signal()
            default:
                break
            }
        }
        listener.newConnectionHandler = { [weak self] connection in
            guard let self else { return }
            Client(connection: connection, server: self).start()
        }
        listener.start(queue: queue)
        guard bound.wait(timeout: .now() + 10) == .success else {
            throw CommandError("listener did not become ready in 10 s")
        }
        listener.stateUpdateHandler = nil
        if let failure { throw failure }
    }

    func stop() {
        listener.cancel()
    }

    /// Constant-time comparison, so the token can't be guessed byte by byte.
    fileprivate func accepts(_ candidate: String) -> Bool {
        let bytes = Array(candidate.utf8)
        guard bytes.count == token.count else { return false }
        var diff: UInt8 = 0
        for (a, b) in zip(bytes, token) { diff |= a ^ b }
        return diff == 0
    }

    fileprivate var networkQueue: DispatchQueue { queue }
}

/// One TCP client. Its state is only touched on the server's network queue.
private final class Client {
    /// Longest accepted line; a client that sends more is disconnected.
    private static let maxLine = 1 << 20

    private let connection: NWConnection
    private let server: Server
    private var buffer = Data()
    private var authenticated = false

    init(connection: NWConnection, server: Server) {
        self.connection = connection
        self.server = server
    }

    func start() {
        connection.start(queue: server.networkQueue)
        receive()
    }

    private func receive() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [self] data, _, isComplete, error in
            if let data { buffer.append(data) }
            drain()
            if isComplete || error != nil {
                connection.cancel()
            } else if buffer.count > Client.maxLine {
                send(["id": NSNull(), "error": ["message": "line too long"]], thenClose: true)
            } else {
                receive()
            }
        }
    }

    private func drain() {
        while let newline = buffer.firstIndex(of: 0x0A) {
            let line = buffer.subdata(in: buffer.startIndex..<newline)
            buffer.removeSubrange(buffer.startIndex...newline)
            if !line.isEmpty { handle(line) }
        }
    }

    private func handle(_ line: Data) {
        guard let object = try? JSONSerialization.jsonObject(with: line),
            let request = object as? [String: Any]
        else {
            send(["id": NSNull(), "error": ["message": "invalid JSON"]], thenClose: !authenticated)
            return
        }
        let id = request["id"] ?? NSNull()
        if !authenticated {
            guard let token = request["token"] as? String, server.accepts(token) else {
                send(["id": id, "error": ["message": "unauthorized"]], thenClose: true)
                return
            }
            authenticated = true
        }
        guard let method = request["method"] as? String else {
            send(["id": id, "error": ["message": "missing method"]], thenClose: false)
            return
        }
        let params = request["params"] as? [String: Any] ?? [:]
        server.inbox.push(Job(method: method, params: params) { [self] result in
            switch result {
            case .success(let value):
                send(["id": id, "result": value], thenClose: false)
            case .failure(let error):
                let message = (error as? CommandError)?.message ?? String(describing: error)
                send(["id": id, "error": ["message": message]], thenClose: false)
            }
        })
    }

    /// Writes one reply line. Safe from any thread: NWConnection serialises sends.
    private func send(_ reply: [String: Any], thenClose close: Bool) {
        var body = reply
        if !JSONSerialization.isValidJSONObject(body) {
            body = ["id": reply["id"] ?? NSNull(), "error": ["message": "result is not valid JSON"]]
        }
        var data = (try? JSONSerialization.data(withJSONObject: body)) ?? Data()
        data.append(0x0A)
        connection.send(content: data, completion: .contentProcessed { [connection] _ in
            if close { connection.cancel() }
        })
    }
}
