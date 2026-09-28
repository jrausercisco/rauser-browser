# Rauser native messaging protocol

`schema.json` is the source of truth for native messages. Regenerate the Rust and
TypeScript types after changing it:

```sh
python3 protocol/generate.py
python3 protocol/generate.py --check
```

The generated files are `protocol/src/generated.rs` and
`protocol/ts/generated.ts`. Both are committed so ordinary builds do not need
Python or a code generation package. The generator uses only Python's standard
library and rejects schema features it cannot translate.

Every request has `protocol_version`, a nonempty `request_id` of at most 128
characters, and a `type` tag. The host checks the version before deserializing
the typed request. A mismatch returns an `error` response with code
`unsupported_protocol_version` and the host's protocol version. If malformed
input has no recoverable request ID, an `error` response may use an empty ID.

The M0 contract covers host discovery and configuration. Additional capture,
notes, and agent messages require a protocol version change. The host remains
responsible for runtime validation of string lengths, absolute paths, storage
confinement, and whether a requested config change is authorized.
