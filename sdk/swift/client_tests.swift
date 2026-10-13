import Foundation

private actor Fixture {
    var requests: [[String: JSONValue]] = []
    let response: String
    let echoId: Bool

    init(_ response: String, echoId: Bool = false) {
        self.response = response
        self.echoId = echoId
    }

    func exchange(_ data: Data) throws -> Data {
        let request = try JSONDecoder().decode([String: JSONValue].self, from: data)
        requests.append(request)
        if echoId {
            var reply = try JSONDecoder().decode([String: JSONValue].self, from: Data(response.utf8))
            reply["id"] = request["id"]
            return try JSONEncoder().encode(reply)
        }
        return Data(response.utf8)
    }
}

@main
struct ClientTests {
    static func main() async throws {
        let fixture = Fixture(#"{"jsonrpc":"2.0","id":1,"result":{"api":1,"library":"fixture","name":"keel","net":false,"pid":42,"version":"test"}}"#)
        let client = KeelClient(transport: { try await fixture.exchange($0) })
        let version = try await client.version([:])
        precondition(version.name == "keel" && version.api == 1)
        let requests = await fixture.requests
        precondition(requests.count == 1)
        precondition(requests[0]["jsonrpc"] == .string("2.0"))
        precondition(requests[0]["id"] == .int(1))
        precondition(requests[0]["method"] == .string("version"))
        precondition(requests[0]["params"] == .object([:]))

        let concurrentFixture = Fixture(#"{"jsonrpc":"2.0","id":0,"result":{"api":1,"library":"fixture","name":"keel","net":false,"pid":42,"version":"test"}}"#, echoId: true)
        let concurrentClient = KeelClient(transport: { try await concurrentFixture.exchange($0) })
        async let first = concurrentClient.version([:])
        async let second = concurrentClient.version([:])
        let versions = try await (first, second)
        precondition(versions.0.name == "keel" && versions.1.name == "keel")
        let concurrentRequests = await concurrentFixture.requests
        precondition(Set(concurrentRequests.compactMap { request -> Int64? in
            guard case .int(let id) = request["id"] else { return nil }
            return id
        }) == Set([1, 2]))

        let tagsFixture = Fixture(#"{"jsonrpc":"2.0","id":1,"result":[{"id":1,"name":"test","color":null}]}"#)
        let tagsClient = KeelClient(transport: { try await tagsFixture.exchange($0) })
        let tags = try await tagsClient.tags_list(TagsListParams())
        precondition(tags.count == 1 && tags[0].color == nil && tags[0].parent == nil)

        // Operations and nested parameter types added since the original SDK registry.
        let namesFixture = Fixture(#"{"jsonrpc":"2.0","id":1,"result":{"paths":["/fixture"],"truncated":true}}"#)
        let namesClient = KeelClient(transport: { try await namesFixture.exchange($0) })
        let names = try await namesClient.files_names(FilesNamesParams(max: 1))
        precondition(names.paths == ["/fixture"] && names.truncated)
        let namesRequests = await namesFixture.requests
        precondition(namesRequests[0]["method"] == .string("files.names"))
        precondition(namesRequests[0]["params"] == .object(["max": .int(1)]))

        let bookmark = ViewsCreateParams(
            columns: ["name", "size"], matching: ViewsCreateParamsMatching(case: true),
            name: "Invoices", query: "invoice"
        )
        let bookmarkJSON = try JSONDecoder().decode(JSONValue.self, from: JSONEncoder().encode(bookmark))
        precondition(bookmarkJSON == .object([
            "columns": .array([.string("name"), .string("size")]),
            "matching": .object(["case": .bool(true)]),
            "name": .string("Invoices"), "query": .string("invoice")
        ]))

        let previewFixture = Fixture(#"{"jsonrpc":"2.0","id":1,"result":{"changes":[],"expires_at":123,"input_hash":"hash","operation":"tags.add","plan_id":"plan","summary":"tag preview","warnings":[]}}"#)
        let previewClient = KeelClient(transport: { try await previewFixture.exchange($0) })
        let preview = try await previewClient.tags_add(TagsAddParams(paths: ["/fixture"], tag: "test"))
        precondition(preview.plan_id == "plan" && preview.input_hash == "hash")
        let previewRequests = await previewFixture.requests
        precondition(previewRequests.count == 1) // No automatic execute.
        precondition(previewRequests[0]["params"] == .object([
            "paths": .array([.string("/fixture")]), "tag": .string("test")
        ])) // Absent optionals stay absent, so Rust supplies defaults.

        let integerFixture = Fixture(#"{"jsonrpc":"2.0","id":1,"result":{"data":"","eof":true,"offset":18446744073709551615}}"#)
        let integerClient = KeelClient(transport: { try await integerFixture.exchange($0) })
        let read = try await integerClient.read(ReadParams(offset: UInt64.max, path: "/fixture"))
        precondition(read.offset == UInt64.max)
        let integerRequests = await integerFixture.requests
        guard case .object(let readParams) = integerRequests[0]["params"] else { fatalError("params") }
        precondition(readParams["offset"] == .uint(UInt64.max))
        let signed = try JSONDecoder().decode(JSONValue.self, from: Data("-9223372036854775808".utf8))
        precondition(signed == .int(Int64.min))

        let errorFixture = Fixture(#"{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"denied","data":{"why":"fixture"}}}"#)
        let errorClient = KeelClient(transport: { try await errorFixture.exchange($0) })
        do {
            _ = try await errorClient.version([:])
            fatalError("expected RPC error")
        } catch let error as RpcError {
            precondition(error.code == -32001 && error.message == "denied")
            precondition(error.data == .object(["why": .string("fixture")]))
        }

        for response in [
            #"{"jsonrpc":"2.0","id":2,"result":{}}"#,
            #"{"jsonrpc":"1.0","id":1,"result":{}}"#,
            #"{"jsonrpc":"2.0","id":1}"#,
            #"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"bad"}}"#
        ] {
            let invalid = Fixture(response)
            let invalidClient = KeelClient(transport: { try await invalid.exchange($0) })
            do {
                _ = try await invalidClient.version([:])
                fatalError("expected protocol error")
            } catch is RpcProtocolError {}
        }

        let nullFixture = Fixture(#"{"jsonrpc":"2.0","id":1,"result":null}"#)
        let nullClient = KeelClient(transport: { try await nullFixture.exchange($0) })
        do {
            _ = try await nullClient.version([:])
            fatalError("expected typed decoding error")
        } catch is DecodingError {} // Null is present, but not a VersionResult.

        let failingClient = KeelClient(transport: { _ in throw RpcProtocolError.invalidResponse })
        do {
            _ = try await failingClient.version([:])
            fatalError("expected transport error")
        } catch RpcProtocolError.invalidResponse {}
        print("Swift client tests passed")
    }
}
