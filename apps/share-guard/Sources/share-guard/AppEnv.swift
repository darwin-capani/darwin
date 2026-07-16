// AppEnv.swift — the launch environment the daemon hands the micro-app (mirrors
// apps/vision/Sources/vision/AppEnv.swift; FROZEN wire contract with the daemon).
//
// Contract (daemon/src/apps.rs): the host passes the per-app socket + capability
// token via the launch ENV ONLY (never argv — argv is world-readable via ps):
//     DARWIN_APP_TOKEN   hex HMAC-SHA256 capability token; stamped on EVERY
//                        app->host line so the host can verify it.
//     DARWIN_APP_SOCKET  absolute path to this app's per-app Unix socket (JSONL);
//                        the app connect()s to it.
//     DARWIN_APP_NAME    the app's name ("share-guard"); matches the manifest +
//                        directory + telemetry "name".
//
// Share Guard needs NO TCC-declared capability (it never opens camera/screen/mic
// — it scrubs a supplied text/image payload), so unlike vision there are no
// camera/screen declaration flags here.

import Foundation

/// The validated launch environment for the Share Guard micro-app.
public struct AppEnv: Sendable, Equatable {
    /// Hex capability token to stamp on every app->host line.
    public let token: String
    /// Absolute path to this app's per-app Unix socket.
    public let socketPath: String
    /// The app's name (expected: "share-guard").
    public let name: String

    public init(token: String, socketPath: String, name: String) {
        self.token = token
        self.socketPath = socketPath
        self.name = name
    }

    /// Env var keys — single source of truth (the daemon writes these exact
    /// names; do not rename without changing apps.rs).
    public enum Key {
        public static let token  = "DARWIN_APP_TOKEN"
        public static let socket = "DARWIN_APP_SOCKET"
        public static let name   = "DARWIN_APP_NAME"
    }

    /// Why the env was unusable — surfaced as a clean exit, not a crash.
    public enum EnvError: Error, Equatable, CustomStringConvertible {
        case missing(String)   // a required key was absent or empty
        public var description: String {
            switch self {
            case .missing(let k): return "required launch env var \(k) is missing or empty"
            }
        }
    }

    /// Load from a key->value dictionary (the real loader passes
    /// ProcessInfo.processInfo.environment; tests pass a literal dict, so parsing
    /// is exercised with NO process env mutation).
    public static func load(from env: [String: String]) throws -> AppEnv {
        func required(_ key: String) throws -> String {
            guard let v = env[key], !v.isEmpty else { throw EnvError.missing(key) }
            return v
        }
        return AppEnv(
            token: try required(Key.token),
            socketPath: try required(Key.socket),
            name: try required(Key.name))
    }

    /// Load from the live process environment.
    public static func loadFromProcess() throws -> AppEnv {
        try load(from: ProcessInfo.processInfo.environment)
    }
}
