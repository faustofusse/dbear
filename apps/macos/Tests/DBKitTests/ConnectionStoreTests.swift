import Foundation
import Testing
@testable import DBKit

private func tempStorePath() -> String {
    FileManager.default.temporaryDirectory
        .appendingPathComponent("dbear-tests-\(UUID().uuidString)/dbear.db").path
}

@Test func storeRoundTripsWithoutPasswords() throws {
    let path = tempStorePath()
    defer { try? FileManager.default.removeItem(atPath: (path as NSString).deletingLastPathComponent) }

    let store = try ConnectionStore.open(path: path)
    #expect(store.connections().isEmpty)

    var draft = ConnectionConfig.blank()
    draft.name = "  prod  "
    draft.database = "app"
    draft.user = "readonly"
    draft.password = "hunter2"
    let saved = try store.upsert(draft)
    #expect(!saved.id.isEmpty && saved.name == "prod" && saved.password == nil)

    // Nothing on disk (database, WAL) contains the password.
    let dir = (path as NSString).deletingLastPathComponent
    for file in try FileManager.default.contentsOfDirectory(atPath: dir) {
        let bytes = try Data(contentsOf: URL(fileURLWithPath: dir).appendingPathComponent(file))
        #expect(bytes.range(of: Data("hunter2".utf8)) == nil, "password leaked into \(file)")
    }
    #expect(try ConnectionStore.open(path: path).connections() == [saved])

    #expect(try store.remove(id: saved.id))
    #expect(store.connections().isEmpty)
}

@Test func storeRejectsInvalidConfig() throws {
    let store = try ConnectionStore.open(path: tempStorePath())
    var noHost = ConnectionConfig.blank()
    noHost.host = " "
    #expect(throws: DatabaseError.self) { try store.upsert(noHost) }
    #expect(noHost.validationError == "Enter a host.")
    // Name and database are optional: the name falls back to the host.
    #expect(try store.upsert(.blank()).name == "localhost")
}

@Test func parsesConnectionURL() throws {
    let c = try ConnectionConfig.parse(url: "postgres://me:p%40ss@db.example.com:6543/shop?sslmode=require")
    #expect(c.host == "db.example.com" && c.port == 6543 && c.database == "shop" && c.name == "shop")
    #expect(c.user == "me" && c.password == "p@ss" && c.sslMode == .require)
    #expect(c.url() == "postgres://me@db.example.com:6543/shop?sslmode=require")
    #expect(DatabaseKind.postgres.defaultPort == 5432)
}

@Test func parsesSqlServerURL() throws {
    let c = try ConnectionConfig.parse(url: "sqlserver://sa@db.example.com:14339/app_dev?encrypt=true&trustServerCertificate=true")
    #expect(c.kind == .sqlServer && c.port == 14339 && c.database == "app_dev" && c.sslMode == .require)
    #expect(c.supportsMultipleDatabases && c.kind.displayName == "SQL Server")
    #expect(c.url() == "sqlserver://sa@db.example.com:14339/app_dev?sslmode=require")
    #expect(DatabaseKind.sqlServer.defaultPort == 1433)
    var blank = c
    blank.database = ""
    #expect(blank.defaultDatabase == "master")
}

@Test func inMemorySecrets() throws {
    let secrets = InMemorySecretStore()
    try secrets.setPassword("x", for: "a")
    #expect(secrets.hasPassword(for: "a") && secrets.password(for: "a") == "x")
    secrets.deletePassword(for: "a")
    #expect(!secrets.hasPassword(for: "a"))
}

@Test func storeRemembersTheLastDatabase() throws {
    let path = tempStorePath()
    defer { try? FileManager.default.removeItem(atPath: (path as NSString).deletingLastPathComponent) }
    let store = try ConnectionStore.open(path: path)
    var draft = ConnectionConfig.blank()
    draft.host = "localhost"
    let saved = try store.upsert(draft)
    #expect(store.lastDatabase(of: saved.id) == nil)
    try store.setLastDatabase("billing", of: saved.id)
    #expect(try ConnectionStore.open(path: path).lastDatabase(of: saved.id) == "billing")
    try store.setLastDatabase(nil, of: saved.id)
    #expect(store.lastDatabase(of: saved.id) == nil)
}
