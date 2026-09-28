# Brauser native messaging protocol

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

Protocol version 4 covers host discovery, configuration, consent, visit
capture, versioned whole-file page notes, and the AI-harness readiness gate. `get_config` returns a
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

Version 4 adds `StorageConfig.summaries_dir` (required, `null` until a
summaries folder is chosen) and four required `ConfigSnapshot` keys:
`agent_denylist` (normalized hostnames), `agent_denylist_confirmed`,
`log_incognito` (always `false`), and `agent` (the harness entry or `null`).
`config_result` also reports `agent_status`, the host's view of whether AI
commands may run. `update_config` requires `harness_token`; only native
harness setup mints one, and it is the only way to set
`agent_denylist_confirmed` or a harness entry. The extension may echo both
unchanged, or remove the harness, which keeps the confirmation. Adding a
denylist entry needs no confirmation; removing or replacing one needs a
`confirm_config` token. `check_agent` runs the readiness gate every AI command
uses and, for a non-null `url`, reports whether the denylist allows that
page; `agent_checked` is its only success response. Every refusal is an
`error`, `not_configured` naming settings. An M1 TOML file loads with
`summaries_dir` null and the denylist unconfirmed.

Native harness setup is two messages. `discover_harnesses` takes
`expected_revision` and returns `harnesses_discovered` with at most eight
offers. Each offer has the binary the host found, its resolved `real_path`,
`version`, argument template, the environment names it requires, and optional
names with a `present` flag; values are never sent. A usable offer carries a
five-minute, single-use `offer_id` bound to the revision. A refused offer
(Codex, for now, or an unsafe path) has a `null` `offer_id` and a `refusal`.
`confirm_harness_setup` names an `offer_id`, the optional environment names
to pass through, the denylist, and `summaries_dir` (`null` keeps the current
folder or uses `summaries`). The host spends the offer, re-checks the
binary's identity, shows a native confirmation listing the program, arguments,
environment names, summaries folder, and every denylist entry, then runs one
short test prompt. Only when the test passes does it return
`harness_setup_confirmed` with the proposed `config`, the confirmed
`summary`, and a single-use `harness_token`. That token authorizes exactly
that `config` at that revision in `update_config`, cannot be combined with a
picker or consent token, and is refused if the binary changed since. A
canceled dialog is `cancelled`; a failed test run is `invalid_config`, and
neither mints a token. The extension never supplies a binary path. The
settings page sends that `config` in `update_config` right away, with the
`harness_token` and null picker and consent tokens; it never sends it through
`confirm_config`.

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

Page notes work on any HTTP(S) page and are not gated by the site allowlist.
`load_note` carries the page's current URL; the host normalizes it the same
way as a logged visit and returns `note_loaded` with `exists`, a `revision`
(`missing` when no note exists yet, otherwise `sha256:` followed by 64
lowercase hex digits of the file's exact bytes), the note's `title`, and its
`body`. `save_note` carries the URL, a `title` (used only when the note does
not exist yet; the host preserves the existing title on every later save,
since the panel only edits the body), the new `body`, and `expected_revision`
from the panel's last load or save. A matching revision replaces the note
whole and returns `note_saved` with a new `revision`, `relative_path`, and an
`outcome` of `created`, `replaced`, `created_with_warning`, or
`replaced_with_warning` (the file is in place but a post-publication step,
such as a directory sync, warned; not a failure and not retried). A stale
`expected_revision` refuses the write and returns `note_conflict` with the
note's current `exists`, `revision`, `title`, and `body` instead, so the
panel can reload the newer note and show the text it could not save. Before
replacing, the host checks the existing file's recorded canonical URL and
`brauser:` ownership (absent or `page` `kind`, never `summary`); a mismatch
is a `conflict` error, and the file is never adopted or overwritten.

The host remains responsible for runtime validation of string lengths,
absolute paths, site origins and path prefixes, storage confinement, token
authority, and whether a requested config change is authorized.
