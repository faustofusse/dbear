import Foundation
import Testing
@testable import DBKit

private let postgresEnabled = ProcessInfo.processInfo.environment["DBEAR_TEST_POSTGRES"] == "1"
private var devDB: ConnectionConfig { Drivers.sampleConnections().first { $0.id == "local-pg" }! }

@Test func accessFeaturesPerDatabase() {
    let pg = Access.features(.postgres)
    #expect(pg?.superuser == true && pg?.hosts == false && pg?.grantsPerDatabase == true)
    #expect(pg?.objectKinds.contains(.allTables) == true)
    #expect(Access.features(.mysql)?.hosts == true)
    #expect(Access.features(.sqlite) == nil)
    #expect(Access.privileges(.postgres, on: .schema) == ["USAGE", "CREATE"])
}

@Test func generatesPasswordsAndLevels() throws {
    let password = try Access.generatePassword()
    let other = try Access.generatePassword()
    #expect(password.count == 24 && password != other)
    #expect(Access.levels(.postgres) == [.noAccess, .connect, .readOnly, .readWrite, .schemaChanges])
    #expect(Access.levels(.mysql).map(\.title) == ["No access", "Read only", "Read and write", "Schema changes"])
    #expect(DatabaseLevel.readOnly.summary(.postgres).contains("created later"))
}

@Test func previewsSeveralChangesInOrder() throws {
    var spec = RoleSpec()
    spec.name = "app"
    let statements = try Drivers.make(for: devDB).previewAccess([
        .createRole(spec),
        .setPrivileges(role: RoleRef(name: "app"), object: .database("postgres"), before: PrivilegeSet(), after: PrivilegeSet(privileges: ["CONNECT"])),
    ])
    #expect(statements.last?.sql == #"GRANT CONNECT ON DATABASE "postgres" TO "app""#)
}

@Test func previewMasksPasswords() throws {
    var spec = RoleSpec()
    spec.name = "reporter"
    spec.password = "hunter2"
    spec.memberOf = [RoleRef(name: "readers")]
    let statements = try Drivers.make(for: devDB).previewAccess(.createRole(spec))
    #expect(statements.map(\.sql) == [
        #"CREATE ROLE "reporter" WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD 'hunter2'"#,
        #"GRANT "readers" TO "reporter""#,
    ])
    #expect(!statements.map(\.display).joined().contains("hunter2"))
}

@Test(.enabled(if: postgresEnabled)) func managesARoleThroughTheFFI() async throws {
    let driver = Drivers.make(for: devDB)
    _ = try await driver.execute("drop role if exists dbear_swift_access")
    var spec = RoleSpec()
    spec.name = "dbear_swift_access"
    spec.canLogin = false
    try await driver.applyAccess(.createRole(spec))
    let role = try #require(try await driver.listRoles().first { $0.name == "dbear_swift_access" })
    #expect(!role.canLogin && !role.isSystem)

    let object = GrantObject.schema("public")
    try await driver.applyAccess(.setPrivileges(role: role.reference, object: object, before: PrivilegeSet(), after: PrivilegeSet(privileges: ["USAGE"])))
    #expect(try await driver.listGrants(of: role.reference) == [ObjectPrivileges(object: object, privileges: PrivilegeSet(privileges: ["USAGE"]))])
    try await driver.applyAccess(.setPrivileges(role: role.reference, object: object, before: PrivilegeSet(privileges: ["USAGE"]), after: PrivilegeSet()))
    #expect(try await driver.listGrants(of: role.reference).isEmpty)

    let access = try await driver.listDatabaseAccess(of: role.reference)
    #expect(access.contains { $0.database == "app_dev" && $0.privileges.isEmpty && $0.everyoneCanConnect })
    let context = try await driver.databaseLevel(of: role.reference, in: "postgres")
    #expect(context.level == .noAccess && context.schemas.contains("public"))
    try await driver.applyAccess(.setDatabaseLevel(role: role.reference, context: context, level: .readOnly))
    #expect(try await driver.databaseLevel(of: role.reference, in: "postgres").level == .readOnly)
    _ = try await Drivers.make(for: devDB.withDatabase("postgres")).execute("drop owned by dbear_swift_access")

    try await driver.applyAccess(.dropRole(role.reference))
    #expect(try await driver.listRoles().contains { $0.name == "dbear_swift_access" } == false)
}
