import Testing
@testable import DBKit

@Test func copiesRowsThroughTheCore() {
    let columns = [
        ColumnInfo(name: "id", typeName: "int8", isPrimaryKey: true, isNullable: false),
        ColumnInfo(name: "name", typeName: "text", isPrimaryKey: false, isNullable: true),
    ]
    let rows: [[DBValue]] = [[.int(1), .text("O'Hara")], [.int(2), .null]]
    #expect(RowFormatter.format(rows, columns: columns, as: .tsv, kind: .postgres, headers: true) == "id\tname\n1\tO'Hara\n2\t")
    #expect(
        RowFormatter.format(rows, columns: columns, as: .insert, kind: .mysql, schema: "shop", table: "people")
            == "insert into `shop`.`people` (`id`, `name`) values (1, 'O''Hara');\ninsert into `shop`.`people` (`id`, `name`) values (2, null);"
    )
}

@Test func prettyPrintsJSONObjectsOnly() {
    #expect(RowFormatter.prettyJSON(#"{"b":1,"a":2}"#) == "{\n  \"b\": 1,\n  \"a\": 2\n}")
    #expect(RowFormatter.prettyJSON("hello") == nil)
}
