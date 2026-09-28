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

The M0 contract covers host discovery and configuration. `get_config` returns a
`revision` that the extension must send as `expected_revision` with its full
`update_config` snapshot. A successful update returns a new `revision`; a stale
revision returns a `conflict` error. The revision is `missing` when no config
file exists, or `sha256:` followed by 64 lowercase hex digits for an existing
file. The host compares it with the current file before replacement.

`ConfigSnapshot.storage` must be present: use `null` only to explicitly clear
the selected notes folder. The generated Rust deserializer rejects an omitted
field, matching the JSON Schema requirement.

Additional capture, notes, and agent messages require a protocol version
change. The host remains responsible for runtime validation of string lengths,
absolute paths, storage confinement, and whether a requested config change is
authorized.
