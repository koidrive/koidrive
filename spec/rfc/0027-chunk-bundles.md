# RFC 0027: Chunk bundles

Status: proposed. Applies to spec v0 (format_version 1). Adds a route next to the chunk relay `PUT`, three messages to `rpc.proto`, and the `chunk_bundles` feature (RFC 0022).

A folder of small files needs one upload request per chunk. With a round trip of 100 ms, the request count, not the bytes, bounds a first sync of a code workspace. A bundle carries several chunks in one request. It is a transport change only: each chunk in a bundle is checked, stored and receipted exactly as a `PUT` of that chunk alone would be. `PrepareUpload`, tickets, incarnations, receipts, holds, `chunk_grace_days` and the recovery rules of RFC 0002 apply per chunk and do not change.

## 1. Messages

`rpc.proto` adds:

- `ChunkBundle { repeated BundledChunk chunks = 1; }`
- `BundledChunk { string target = 1; bytes data = 2; }`. `target` is the `Target.url` of the chunk's `ChunkPlan`, unchanged. `data` is the chunk's bytes.
- `ChunkBundleResult { repeated BundledChunkResult results = 1; }`
- `BundledChunkResult { bytes hash = 1; uint32 status = 2; UploadReceipt receipt = 3; Error error = 4; }`

## 2. Route

`POST /v1/spaces/{space_id}/chunks` with a `ChunkBundle` body and `Content-Type: application/x-protobuf`. Like a chunk `PUT`, it carries no bearer token: each chunk's ticket authorizes it.

The server refuses the whole request, and stores nothing, when:

| Condition | Status, code |
|---|---|
| `Content-Length` is missing | 411, `INVALID_ARGUMENT` |
| `Content-Length` or the body read is over 4 MiB | 413, `LIMIT_EXCEEDED` |
| The body is not a `ChunkBundle` | 400, `INVALID_ARGUMENT` |
| It holds 0 chunks or more than 32 | 413, `LIMIT_EXCEEDED` |
| A `target` is not a URL whose path is `/v1/spaces/{space_id}/chunks/{hash}` for the route's `space_id`, with a valid `inc` | 400, `INVALID_ARGUMENT` |
| Two chunks name the same hash | 400, `INVALID_ARGUMENT` |
| A chunk's `data` is empty or over 1 MiB | 413, `LIMIT_EXCEEDED` |

Otherwise the server answers 200 with a `ChunkBundleResult` that has one result per chunk, in request order. It handles each chunk as `PUT` of `data` to `target` with `Content-Length` set to the length of `data`: the same ticket check (`put` grant over space, hash, incarnation and size), the same storage write with the hash checked by storage and no overwrite of an existing object, and the same receipt. `status` is the HTTP status that `PUT` would have answered. A 2xx result carries `receipt`; any other carries `error`. One chunk's failure does not change another chunk's result.

At most 6 storage writes run at once, which is the Workers limit on simultaneous outgoing connections ([Workers limits](https://developers.cloudflare.com/workers/platform/limits/)). The cap of 32 chunks keeps a bundle inside the 50 subrequests a Workers Free request may make (same page), counting each storage write as one.

## 3. Feature

A Space DO whose code implements this RFC lists `chunk_bundles` in `Welcome.features`. RFC 0022 §2 registers the name.

## 4. Client rules

1. A client sends bundles on a connection only while its `Welcome` lists `chunk_bundles`.
2. A client bundles only chunks whose `Target` has method `PUT` or none, no headers, and a URL path that ends in `/v1/spaces/{space_id}/chunks/{hash}` with the chunk's own hash. It sends the bundle to that URL with the last path segment, the query and the fragment removed. A presigned storage target is never bundled.
3. A client handles each `BundledChunkResult` as it handles the answer to a `PUT` of that chunk with the same status and body: a receipt is used at commit time, 401 and 403 ask `PrepareUpload` again, and a status that a `PUT` retries is retried for that chunk alone, with the same per-chunk attempt limit. A chunk that a `PUT` would fail for good ends the upload.
4. A transport failure, a timeout, or a whole-request status that a `PUT` would retry resends the bundle. Resending is safe: the server writes the same key and does not overwrite an object that exists.
5. A 404 or 405 for the route means the Worker does not have it, for example during the 300 seconds after a deploy (RFC 0022 §3 rule 2). The client sends that bundle's chunks as `PUT`s and sends no more bundles until its next `Welcome`. Any other whole-request refusal sends the bundle's chunks as `PUT`s.

## 5. Compatibility

- A client without this RFC never calls the route, so nothing it sees changes.
- A server without this RFC sends no `chunk_bundles`, so a client with this RFC sends one `PUT` per chunk.

## 6. Tests

- Server: a bundle stores each chunk and its receipts are accepted by a commit; each chunk of a bundle with a bad ticket, wrong bytes or a size its ticket does not cover gets the same status as a `PUT` of it alone, and only the valid chunks are stored; a storage failure fails only its chunk with the `PUT` status; each refusal in §2 stores nothing; a bundle of 32 makes 32 writes with at most 6 at once.
- Client: bundles of at most 32 when offered; `PUT`s when not offered; `PUT`s and no more bundles after a 404; each per-chunk status gives the same outcome as a `PUT` with that answer; only refused chunks are sent again.

## 7. Alternatives considered

1. **A multipart or length-prefixed body.** A protobuf message keeps one schema for both ends, and the runtime decodes `bytes` fields as views into the request body.
2. **A bundle ticket from `PrepareUpload`.** It would move a check into the Space DO and change `PrepareUploadResult`. Per-chunk tickets already authorize each write.
3. **Larger chunks or packing files into one object.** That changes content addressing, deduplication and garbage collection, which a transport change leaves alone.
