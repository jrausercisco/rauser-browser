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

Protocol version 2 covers host discovery, configuration, consent, visit capture,
and create-only page notes. `get_config` returns a
`revision` that the extension must send as `expected_revision` with its full
`update_config` snapshot. A successful update returns a new `revision`; a stale
revision returns a `conflict` error. The revision is `missing` when no config
file exists, or `sha256:` followed by 64 lowercase hex digits for an existing
file. The host compares it with the current file before replacement. An invalid
or newer TOML file yields an inert empty snapshot, its raw-file revision, and a
`config_issue` in `hello_result` and `config_result`. A revision-checked repair
backs up those exact bytes before replacing the file.

`ConfigSnapshot.storage` must be present: use `null` only to explicitly clear
the selected notes folder. `sites`, `strip_params`, and `near_repeat_secs` are
also required. The generated Rust deserializer rejects omitted nullable fields,
matching the JSON Schema requirement. Existing M0 TOML defaults to an empty
site list, `utm_*`/`fbclid`/`gclid` stripping, and a 300-second repeat window.
An M0 file with a notes root but no native-picker provenance is returned as an
inert repair state. The user must reselect the folder; the host backs up the
old file before saving the M1 config. M1 accepts the neutral profile only.

`choose_folder` opens the native OS directory picker. A successful
`folder_chosen` returns a canonical path and a five-minute, single-use
`picker_token` bound to the current revision and path. `update_config` requires
that token when `storage.root` changes. The extension cannot authorize a raw
root path. Picker cancellation leaves config unchanged.

`confirm_config` supplies the expected revision, exact proposed snapshot, and
picker token when the root changes. The host presents its own confirmation for
capture expansion, including a new or wider site rule, enabling logging,
retaining formerly stripped query parameters, or shortening repeat suppression.
Changing the profile or any content location within the chosen folder also
needs native confirmation, so an extension cannot silently redirect writes.
`config_confirmed` carries a five-minute, single-use `consent_token` bound to
the current snapshot, revision, and exact proposed snapshot. `update_config`
requires it for these changes. Both tokens are required-nullable fields: send
`null` when one is unnecessary. Narrowing capture can proceed immediately.

`record_visit` carries one immutable `VisitEvent` with a UUID event ID,
original URL, nullable title, timestamp, and incognito flag. The timestamp
is RFC 3339 with seconds precision and either `Z` or the local `+HH:MM`/`-HH:MM`
offset; the host files the visit under that offset's calendar date. The host
checks the original URL against its confirmed site policy and returns a
`visit_recorded` outcome of `persisted`, `suppressed`, `rejected`, or
`retryable`; only the first three are terminal for the extension queue.
`create_page_note` carries the original URL, title, and user-authored body;
`page_note_result` distinguishes `created`, `already_present`, `conflict`, and
`created_with_warning`.

The host remains responsible for runtime validation of string lengths,
absolute paths, site origins and path prefixes, storage confinement, token
authority, and whether a requested config change is authorized.
