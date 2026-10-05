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

    try await driver.applyAccess(.dropRole(role.reference))
    #expect(try await driver.listRoles().contains { $0.name == "dbear_swift_access" } == false)
}
