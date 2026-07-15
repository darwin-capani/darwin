// AppEnvTests.swift — launch-env parsing tests. Driven with literal dicts (no
// process env mutation), so the required-key contract is exercised hermetically.

import XCTest
@testable import share_guard

final class AppEnvTests: XCTestCase {

    func testLoadsAllRequiredKeys() throws {
        let env = try AppEnv.load(from: [
            "DARWIN_APP_TOKEN": "deadbeef",
            "DARWIN_APP_SOCKET": "/tmp/share-guard.sock",
            "DARWIN_APP_NAME": "share-guard",
        ])
        XCTAssertEqual(env.token, "deadbeef")
        XCTAssertEqual(env.socketPath, "/tmp/share-guard.sock")
        XCTAssertEqual(env.name, "share-guard")
    }

    func testMissingTokenThrows() {
        XCTAssertThrowsError(try AppEnv.load(from: [
            "DARWIN_APP_SOCKET": "/tmp/s.sock",
            "DARWIN_APP_NAME": "share-guard",
        ])) { err in
            XCTAssertEqual(err as? AppEnv.EnvError, .missing("DARWIN_APP_TOKEN"))
        }
    }

    func testEmptyValueIsMissing() {
        XCTAssertThrowsError(try AppEnv.load(from: [
            "DARWIN_APP_TOKEN": "",
            "DARWIN_APP_SOCKET": "/tmp/s.sock",
            "DARWIN_APP_NAME": "share-guard",
        ])) { err in
            XCTAssertEqual(err as? AppEnv.EnvError, .missing("DARWIN_APP_TOKEN"))
        }
    }

    func testMissingSocketAndNameThrow() {
        XCTAssertThrowsError(try AppEnv.load(from: [
            "DARWIN_APP_TOKEN": "t",
            "DARWIN_APP_NAME": "share-guard",
        ])) { err in
            XCTAssertEqual(err as? AppEnv.EnvError, .missing("DARWIN_APP_SOCKET"))
        }
        XCTAssertThrowsError(try AppEnv.load(from: [
            "DARWIN_APP_TOKEN": "t",
            "DARWIN_APP_SOCKET": "/tmp/s.sock",
        ])) { err in
            XCTAssertEqual(err as? AppEnv.EnvError, .missing("DARWIN_APP_NAME"))
        }
    }
}
