# Rauser Browser Browsing Assistant — Design Specification

> Status: Draft v0.5 · M0 merged, M1 development implementation in progress, no public package yet · Short name "Rauser"; CLI and binary `rauser` · License: Apache-2.0 · Platforms: Google Chrome on macOS and Windows

## 1. Purpose

Rauser turns the browser into a place where context accumulates instead of disappearing. It runs as an always-on sidebar that:

- **Logs** important sites the user visits
- **Summarizes** pages into notes using the user's own local AI agent
- **Annotates** sites with persistent, per-page notes
- **Saves** pages for later
- **Surfaces** related pages from the user's history and notes
- Provides an **omnibar** for commands and free-text workflows, usually operating on the current page

Captured notes, browse logs, and read-later items are plain Markdown in a folder the user chooses. Operational config and a rebuildable search index stay in standard per-user OS locations. No cloud service, no account, no telemetry.

## 2. Design Principles

1. **The user owns the data.** Markdown files are the source of truth. Everything else, such as the search index, is a rebuildable cache.
2. **Least privilege everywhere.** The extension cannot touch the filesystem. The host cannot do anything it isn't configured to do. Sites are opt-in.
3. **Page content is untrusted.** Any page the user visits may contain hostile content, including prompt injection aimed at the agent.
4. **Never clobber user edits.** Rauser only modifies what it owns.
5. **Format-neutral, convention-friendly.** Output works in any markdown tool. Obsidian is a first-class preset, not a dependency.
6. **Configuration over code.** Agent harnesses, site adapters, and omnibar commands are defined in config, not hardcoded.
7. **Assume nothing about the user's setup.** No default vault location, agent, folder structure, or site list. Rauser ships with examples, and the user makes every choice explicitly at setup. It never writes into a folder the user hasn't chosen.
8. **No always-running native service.** The host runs only while the side panel or settings page is open. If the user enables site logging, the browser's event-driven extension service worker may wake on navigation while the browser is open to buffer allowlisted visits; nothing runs when the browser is closed.

## 3. Architecture

```
┌──────────────────────────────┐        native messaging        ┌──────────────────────────────┐
│  Browser extension (TS, MV3) │  ◄──── JSON, versioned ────►   │  Native host (Rust)          │
│                              │                                │                              │
│  • Sidebar UI + omnibar      │                                │  • Policy enforcement        │
│  • Navigation observation    │                                │  • Vault read/write          │
│  • Content extraction        │                                │  • Search index (SQLite FTS5)│
│  • No secrets, no FS access  │                                │  • Agent invocation          │
└──────────────────────────────┘                                │  • Config owner              │
                                                                └──────────┬───────────────────┘
                                                                           │ argv, no shell
                                                             ┌─────────────┴─────────────┐
                                                             │  Local agent harness       │
                                                             │  (Claude Code, Codex, CLI) │
                                                             └───────────────────────────┘
```

### 3.1 Browser extension

- TypeScript in strict mode, Manifest V3, bundled with a pinned build toolchain.
- Targets Google Chrome Stable on macOS and Windows for the first release. Other browsers are out of scope.
- Sidebar via the Chromium `sidePanel` API for status, pending visits, and page notes. The side panel's header has a settings (gear) button, and the panel shows a warning with a link to settings until a notes folder, at least one site, and capture are configured.
- A dedicated settings page (`options_ui`, opened in a tab) holds configuration: notes folder, enabled sites, and pause. A full tab hosts Chrome's permission prompt and the native dialogs better than the narrow panel. The panel keeps a one-click pause as an emergency stop.
- Holds UI state only. It stores no secrets and makes no network requests to external services.
- No remote code: no `eval`, no dynamically loaded scripts, and a strict extension CSP.

### 3.2 Native host

- One Rust host binary per supported OS and CPU architecture, with `#![forbid(unsafe_code)]` in all first-party crates.
- The only component with filesystem and process privileges.
- Treats every inbound message as untrusted, because a compromised page or extension context could influence it.
- Registered through a native messaging manifest whose `allowed_origins` lists only the official extension IDs.

**Lifecycle.** The host runs only while the side panel or settings page is open. It is never a daemon, service, or login item. Each of those pages opens its own native messaging connection when it loads, the browser spawns a host process for it, and that host exits when the connection closes (stdin EOF), which happens when the page closes. Work the host would otherwise do in the background, such as catching up on external edits to notes, happens on connect instead (see §5.5).

While the panel is closed, the extension's service worker still observes navigation on enabled sites and buffers qualifying visits in `chrome.storage.local`. Each record has a stable event ID, original HTTP(S) URL, optional title, and timestamp. The buffer is capped by count and bytes. Before writing any record, the worker awaits `setAccessLevel({accessLevel: "TRUSTED_CONTEXTS"})`, loads a revisioned, host-confirmed site policy with a bounded expiry, and checks the current Chrome grant; absent or expired policy fails closed. The host independently enforces its confirmed site allowlist and privacy rules against the original URL before any lossy normalization. The host cannot inspect Chrome's live grants. Events leave the buffer only after a terminal host acknowledgement (`persisted`, `suppressed`, or permanent-policy `rejected`); transient errors retain the event for bounded retry and appear in the panel alongside rejections and queue overflow. An invalid or newer host config suspends the extension policy without deleting buffered visits; the panel offers an explicitly confirmed discard action during repair. Disabling logging or confirming a site grant revocation purges affected records. The side panel and settings page serialize config changes and Chrome grant cleanup across every open window through one shared lock, and recheck the grant before enabling capture. The worker accepts messages only from those two extension pages. Each page watches the worker's policy lease and re-reads the host when another page publishes a different revision. An explicit re-enable clears a prior revocation only when the worker has the committed host revision and Chrome currently grants that origin. Summaries and other agent tasks run only while the panel is open.

### 3.3 Why this split

The extension runs in a hostile environment (arbitrary web pages), and the host holds real privileges (files, processes). Putting all policy in the host means a bug in the extension cannot escalate into arbitrary file writes or command execution. Rust gives memory safety on the side of the boundary where it matters most.

## 4. Security Model

### 4.1 Threats in scope

| Threat | Mitigation |
|---|---|
| Malicious page influences extension messages | Host validates and authorizes every request; extension is not trusted |
| Path traversal / symlink escape out of the vault | Host builds all paths from its own slugs; canonical-root confinement; capability-based FS access (e.g. `cap-std`) |
| Command injection via agent invocation | Harnesses run via argv with no shell; page content passed on stdin, never interpolated into argv |
| Prompt injection from page content | Content delimited and labelled untrusted; agent run in its most restricted mode for read-only tasks; outputs written only to Rauser-owned locations |
| Sensitive pages sent to an agent | Agent denylist enforced in the host, overriding explicit user requests with a warning |
| Unwanted browsing surveillance | Logging is opt-in per domain; incognito never logged |
| Other local extensions talking to the host | `allowed_origins` restricted to official extension IDs |
| Supply chain compromise | Pinned dependencies, `cargo-audit`/`cargo-deny`, npm lockfile audit, signed releases with provenance |

### 4.2 Threats out of scope

- A compromised local user account or OS
- A compromised agent harness binary
- Malicious config files written by the user

### 4.3 Host message handling

- Messages are length-prefixed JSON. M0 caps extension-to-host bodies at 4 MiB and host-to-extension bodies at 900 KiB, below Chrome's 1 MiB outbound limit.
- Deserialized into typed structs with `deny_unknown_fields`. Unknown message types are rejected.
- Every message carries a protocol version and request ID. Version mismatch returns a structured error.
- Parser and path handling are fuzzed continuously (see §10).

### 4.4 Filesystem rules

- The host stores the selected folder's filesystem identity after the native picker. Each privileged operation opens a root-scoped directory handle and checks that identity; a moved or replaced folder requires reselection, including repair at the same path.
- Filenames are generated by the host from normalized slugs. The extension never supplies a raw path.
- M0 creates new page files only: write a temp file in the same directory, fsync it, then publish with an atomic no-clobber hard link. It never replaces an existing file.
- M1 never automatically replaces an existing page note. A proposed change to an existing file becomes a no-clobber sibling artifact for user review. Later automatic updates require a proven cross-platform publication step that atomically preserves the exact displaced bytes; a check followed by a rename is insufficient when external editors ignore advisory locks.
- Destructive operations (delete, overwrite outside a managed block) require explicit user confirmation in the sidebar.

### 4.5 Agent invocation rules

- Each harness is a config entry: absolute binary path, argument template, and environment allowlist.
- Spawned with a cleared environment plus allowlisted variables, a timeout, an output size cap, and cancellation support.
- Page content goes on stdin inside a delimited block with an explicit instruction that it is untrusted data.
- For summarization and Q&A, harnesses should be configured in a mode without shell or file-writing tools. The host writes results itself; the agent never writes into the vault directly.
- The agent denylist (banking, HR, health portals, etc.) blocks content from those domains from ever reaching an agent.

## 5. Features

### 5.1 Site logging

- Logging is **opt-in**: the user enables a site and grants its exact origin from a setup action. The host independently checks scheme, host, and any adapter path rule before recording a visit; Chrome's origin permission does not enforce URL paths.
- Incognito and private windows are never logged, regardless of config. The extension manifest uses `incognito: "not_allowed"`, and the event handler resolves the tab's incognito state and rejects it defensively.
- M1 observes main-frame `webNavigation.onCommitted` and `onHistoryStateUpdated` events, using document identity and lifecycle to avoid subframes, redirects, and duplicate SPA records. It does not keep dwell or scroll timers only in service-worker memory. A committed document's title arrives after commit, so the worker briefly holds a new untitled visit and fills its title from the tab's title update; the event is frozen once handed to the panel because the host hashes it for interrupted-write recovery. Each visit carries its local UTC offset, and the daily log uses that local calendar date.
- M1 normalization drops fragments and configured tracking parameters after host authorization. Later opt-in site adapters may apply site-specific transforms (for example, collapsing a GitHub file view to its repository).
- Deliveries are idempotent by event ID recorded in the Markdown log. The host keeps a durable per-event intent in its OS config directory to find the original daily log across date and `log_dir` changes; only a complete Markdown marker proves persistence. Shared locks cover the current policy check, intent, append, and file sync, so revocation and concurrent panel connections cannot race a write. An interrupted append can resume only from an exact byte prefix in the still-selected folder; changed or missing logs require review. Near-repeat visits within a configurable window are suppressed rather than rewriting a prior Markdown entry; the daily log remains append-only. Dwell-time and scroll-depth filters follow M1 only after their state can survive service-worker suspension.

### 5.2 Summaries

1. The extension extracts readable content (Readability-style) from the active tab on demand.
2. The host checks the denylist, then invokes the configured harness with the summary prompt.
3. Output streams back to the sidebar in chunks, staying under the 1 MB native messaging limit per message.
4. The final summary is written into the page note's managed block.

### 5.3 Page notes

- One markdown file per normalized URL.
- The user edits notes in the sidebar or in any external editor. Both are first-class.

### 5.4 Read later

- `/save` writes a read-later entry with an optional agent summary.
- Items can be marked done, which moves them in the index but never deletes the file.

### 5.5 Related pages

- The host maintains a SQLite FTS5 index over titles, summaries, notes, and tags.
- When the active tab changes, the sidebar shows related pages ranked by text relevance, shared tags, and shared domain.
- v1 is keyword search. Embedding-based similarity is a later, optional stage.
- The index lives in the OS cache directory, not in the user's notes folder, and can be deleted and rebuilt at any time with `rauser reindex`.
- On each connect, the host does an incremental scan (by modification time) to pick up files edited outside Rauser. While connected, it watches the notes folder for changes. There is no scheduled reindexing.

### 5.6 Omnibar

Input is either a slash command or free text.

| Command | Action |
|---|---|
| `/summarize` | Summarize the page into its note |
| `/note <text>` | Append text to the page note |
| `/save` | Add the page to read later |
| `/related` | Show related pages |
| `/log` | Force-log the current page, ignoring thresholds |
| free text | Send to the agent with the page as context; show the answer in the sidebar |

**Custom commands** are defined in config:

```toml
[[commands]]
name = "actions"
description = "Extract action items from this page"
prompt = """
Extract action items from the page below. Return a markdown checklist.
"""
include = ["page.content", "selection"]
output = "page_note"        # sidebar | page_note | new_note | clipboard
```

Template variables: `{page.title}`, `{page.url}`, `{page.content}`, `{selection}`, `{input}`, `{date}`.

## 6. Data Format

### 6.1 Folder layout

The user chooses the notes folder at setup. It can be empty, or a subfolder of an existing knowledge base. The three content locations are configurable; the layout below is the suggested starting point offered at setup, not a requirement.

```
<notes folder>/                  chosen by the user, no default
  log/2026/09/2026-09-27.md      daily browse log
  pages/<slug>.md                page note: summary + user notes
  later/<slug>.md                read-later items
```

Rauser's own files live outside the notes folder, in standard OS locations, so it never adds hidden folders to someone's knowledge base:

| File | Location |
|---|---|
| `config.toml` | OS config directory (e.g. `~/Library/Application Support/Rauser` on macOS, `%APPDATA%\Rauser` on Windows) |
| `visit-ids/` | OS config directory; M1 keeps one synced intent file per persisted visit |
| `index.db` | OS cache directory |

If Rauser finds an existing folder with files it didn't create, it leaves them alone. It only indexes files under its configured content locations.

### 6.2 Internal model

The host works on a single `Note` type: `url`, `canonical_url`, `title`, `created`, `updated`, `tags`, `links`, `summary`, `body`. Profiles only control serialization.

### 6.3 Profiles

The **neutral** profile (default) uses YAML frontmatter, relative markdown links, and frontmatter tag lists. It renders correctly on GitHub, in VS Code, and in Obsidian.

The **obsidian** preset switches to `[[wikilinks]]` and Obsidian-compatible daily-note paths.

Custom profiles can extend either one.

M1 implements the neutral serializer only. The host rejects other profile selections until their distinct paths and link styles are implemented and verified.

```toml
[storage]
root = "/path/chosen/at/setup"       # required, no default
profile = "neutral"                  # M1; obsidian and custom profiles are planned
log_dir = "log"
pages_dir = "pages"
later_dir = "later"

[profiles.obsidian]
links = "wikilink"
tags = "frontmatter"
daily_log = "log/{YYYY}/{MM}/{YYYY-MM-DD}.md"
filename = "{title-slug}-{url-id}"       # stable URL-derived suffix is mandatory

[profiles.custom]
extends = "neutral"
frontmatter = "toml"
```

**Reading is lenient, writing follows the profile.** The indexer parses both link styles and any frontmatter format. Switching profiles never rewrites existing files; `rauser migrate --profile <name>` converts them and defaults to a dry run.

### 6.4 Managed blocks

Rauser owns only:

- Its own frontmatter keys, namespaced under `rauser:`
- Content between `<!-- rauser:start -->` and `<!-- rauser:end -->`

Everything else, including unknown frontmatter keys and anything the user writes, is preserved byte for byte. In M1, the host creates new notes with a managed block and never replaces an existing note. A proposed change to an existing note, including one with absent markers, is written as a no-clobber sibling artifact for user review. Malformed or duplicate markers produce a conflict. Automatic managed-block replacement is deferred until a cross-platform atomic displaced-byte backup and conflict procedure is proven with an external-editor race test.

Page-note filenames include a stable identifier derived from the normalized canonical URL, even when a profile uses a title slug. Before updating a file, the host verifies the recorded canonical URL and Rauser ownership match the requested page. An absent or different identity is a conflict, never an opportunity to adopt or overwrite an unrelated file.

### 6.5 Example page note (neutral profile)

```markdown
---
title: Designing Data-Intensive Applications — Chapter 5
url: https://example.com/ddia/ch5
tags: [replication, databases]
rauser:
  canonical_url: https://example.com/ddia/ch5
  url_id: 65fc5f8734006f2e6ac748dc5daadc458ac24dc3fc98c2233160c601c42389a7
  created: 2026-09-27T10:14:00Z
  updated: 2026-09-27T10:15:12Z
  visits: 3
---

<!-- rauser:start -->
## Summary
Leader-based replication trades write availability for consistency...
<!-- rauser:end -->

## My notes
Compare this with how our event pipeline handles failover.
```

## 7. Configuration

Config lives in the OS config directory (§6.1) and is owned by the host. The extension reads and edits it only through host messages, which validate every change and require the revision returned by `get_config`. In M1, a change to `storage.root` requires a short-lived, single-use token from a native folder picker. `update_config` must reject an arbitrary root path supplied by the extension. Adding or widening a site matcher, enabling capture, or weakening a privacy exclusion also requires a native host confirmation and a short-lived, single-use token bound to the exact config delta and current revision. Narrowing or disabling capture can take effect immediately. A Chrome grant is a separate browser access gate and does not authorize a host config change. Canceling a picker or confirmation leaves config unchanged. A malformed or newer on-disk config must produce a recoverable setup error rather than preventing the host from responding at all.

The config starts nearly empty. Setup writes only what the user chooses; everything else is either off or an inert, commented-out example. The host refuses to perform an action whose required config is missing and tells the extension what to configure, rather than guessing. Configuration is edited on the extension's settings page; the side panel links to it.

### 7.1 Agent harnesses

An agent is optional. Without one, logging, notes, read later, and related pages all still work; only AI commands are disabled.

Rauser ships adapter templates for Claude Code, Codex, and a generic CLI, but configures none of them automatically. At setup, the host may look for known harnesses on `PATH` and offer what it finds; the user confirms the binary path and chooses one. Nothing is enabled without that confirmation.

```toml
[agent]
default = "claude-code"              # whichever the user chose; unset means no agent

[agent.harnesses.claude-code]
binary = "/usr/local/bin/claude"
args = ["-p", "{prompt}"]            # illustrative; verify against harness docs
env_allow = ["HOME", "PATH"]
timeout_secs = 120

[agent.harnesses.codex]
binary = "/usr/local/bin/codex"
args = ["exec", "{prompt}"]          # illustrative; verify against harness docs

[agent.harnesses.generic]
binary = "/path/to/any-cli"
args = ["{prompt}"]
stdin = "page"
```

`{prompt}` is the command prompt only. Page content always goes on stdin, never into argv.

### 7.2 Site adapters

```toml
[[sites]]
name = "github"
match = ["https://github.com/*"]
normalize = "repo"                   # collapse to owner/repo
log = true

[[sites]]
name = "jira"
match = ["https://*.atlassian.net/browse/*"]
title_selector = "h1"
log = true
```

The site-adapter examples above are planned after M1. M1 accepts only user-chosen exact origins and optional path prefixes, without site-specific title extraction or URL transforms. Later templates remain disabled until the user enables them and can be edited for self-hosted sites. A wildcard template must be resolved to an exact scheme, host, and port before it is enabled. Enabling an exact site requests its matching optional Chrome origin grant and separate native-host confirmation; the host enforces any narrower path rule itself.

### 7.3 Privacy

```toml
[privacy]
log_incognito = false                # cannot be set to true
agent_denylist = []                  # user-defined; setup offers suggested categories
strip_params = ["utm_*", "fbclid", "gclid"]
```

The denylist starts empty rather than shipping a guess at what the user considers sensitive. Setup prompts the user to add domains and offers suggested categories (banking, email, HR, health) as examples to adapt.

## 8. Extension Permissions

| Permission | Why |
|---|---|
| `sidePanel` | Always-on sidebar |
| `nativeMessaging` | Talk to the host |
| `storage` | UI preferences and the capped visit buffer (§3.2) |
| `webNavigation` | Optional in M1; requested only when the user enables logging |
| `optional_host_permissions` | Manifest declares HTTP(S) patterns for sites discovered during setup; Chrome grants only the exact origin requested for an enabled site. Adapter paths remain host-enforced |

The manifest's broad optional HTTP(S) patterns permit runtime requests for user-chosen sites; no origin is granted at install time. The permission request runs directly from a settings-page button while Chrome still has the user gesture, before awaiting native confirmation. If native confirmation is canceled, the extension removes any newly granted origin and optional API permission. Removing a grant stops capture for that origin and purges its buffered events. The service worker reads the title from tab metadata on granted sites; M1 requests neither `scripting` nor `activeTab`. Content extraction and its separate permissions belong to M2.

No broad host permissions are requested at install time.

## 9. Repository Layout

```
/extension        TypeScript MV3 extension
/host             Rust native host (workspace: protocol, vault, index, agent, cli)
/protocol         Shared message schema (JSON Schema → generated TS and Rust types)
/installers       macOS and Windows user installers; optional Homebrew formula
/fixtures         Sample vault and captured pages for tests
/docs             DESIGN.md, SECURITY.md, CONTRIBUTING.md, config reference
```

The message schema is defined once and code-generated for both sides, so the extension and host cannot drift.

The layout above is the planned release layout. M0 currently contains `protocol/`, `host/`, and CI; the extension, installers, fixtures, and product docs arrive in later milestones.

## 10. Quality

- **Host:** unit tests, property tests on URL normalization and slug generation, and fuzzing (`cargo-fuzz`) on the message parser, frontmatter parser, and path handling.
- **Extension:** unit tests, plus Playwright end-to-end tests running the real extension against a real host and a fixture vault.
- **Round-trip tests:** every profile writes, reads back, and re-writes with no diff, and user content outside managed blocks survives unchanged.
- **CI on every PR:** `clippy -D warnings`, `rustfmt`, `cargo-deny`, ESLint, TypeScript strict, dependency review, and the full test suite on macOS and Windows.

## 11. Distribution

- **Download experience:** provide one public Rauser download page with two clearly labeled steps: install the browser extension from the Chrome Web Store, then download and run the signed native-host installer for the user's OS. The page explains why both parts are needed, links to supported-browser instructions, and offers a short troubleshooting path. The native host is a companion installer; do not ask users to build from source, use a terminal, or install Homebrew to get started.
- **Extension:** publish the stable release through the Chrome Web Store. Keep the published extension ID stable because the native-host manifest authorizes that exact ID. Chrome Web Store installation is the supported path on Windows and macOS; do not instruct users to install a local CRX.
- **Supported browser:** Google Chrome Stable only, on macOS and Windows. Test the current and previous two stable major versions and publish the tested range on the download page. Edge, Brave, Arc, other Chromium browsers, Firefox, and Safari are unsupported in the first release. Chrome's Side Panel API requires Chrome 114 or later; the release floor must also satisfy the current three-version support window.
- **macOS package:** offer a signed and notarized per-user `.pkg` for macOS 15 Sequoia and later, with a universal Intel and Apple silicon host binary and a Chrome user-level native-messaging manifest. Sign the app with the publisher's Developer ID Application identity and the installer package with its Developer ID Installer identity. Homebrew may be offered as an additional channel for technical users, not as the primary consumer install path.
- **Windows package:** offer a signed per-user x64 installer for Windows 11 version 25H2 and later supported releases. Register the native host under `HKEY_CURRENT_USER` for Google Chrome only, without administrator rights. Sign the installer and binaries with a trusted Authenticode code-signing certificate and timestamp the signatures. Windows 10 and Windows on ARM are unsupported in the first release. Document what happens when organizational policy blocks per-user installation.
- **Native-host registration:** register only Google Chrome, using its documented user-level manifest location on macOS and registry entry on Windows. The installer must use the stable Chrome Web Store extension ID in `allowed_origins`; it must never use wildcard origins. `rauser register` may repair the Chrome registration after installation, with a clear confirmation and status report. Uninstallation removes only Rauser's host registration and installed binaries.
- **First-run flow:** after the extension is installed, opening the side panel checks for the host and gives a direct OS-specific download link if it is missing. After host installation, the panel verifies host/protocol compatibility, warns that setup is incomplete, and links to the settings page, which guides the user through choosing a notes folder, profile, optional agent, optional sites, and privacy exclusions. Explain permissions at the point they are requested. Show a final review of what will be captured and where it will be written; capture stays off until the user finishes setup. Include a no-agent setup path that reaches a usable capture experience.
- **Upgrade and removal:** browser-store updates apply to the extension; host updates use the signed OS installer and preserve the configured notes folder. Show a clear incompatibility error if extension and host protocol versions do not match. Uninstall leaves the user's notes and config untouched by default, and the documentation explains how to remove them manually.
- **Release page and support:** use the public GitHub repository's Releases page as the canonical download page. Each release links to the Chrome Web Store listing, the macOS `.pkg`, Windows x64 installer, checksums, install/upgrade/rollback/uninstall instructions, troubleshooting, and the GitHub Issues support channel. Do not publish a release until both OS installers and the extension have passed the install and uninstall checks.
- **Release artifacts:** publish signed installers, checksums, SLSA build provenance, and an SBOM for every release. Reproducible builds are a stretch goal.

## 12. Build Order

| Milestone | Scope |
|---|---|
| **M0 — Foundation** | Protocol schema, host skeleton, config loading, vault confinement, atomic writes, CI |
| **M1 — Capture** | Sidebar shell, site logging, URL normalization, daily log, page notes with managed blocks |
| **M2 — Agent** | Harness adapters, streaming, `/summarize`, denylist enforcement |
| **M3 — Related** | FTS5 index, related pages, read later, `reindex` |
| **M4 — Omnibar** | Built-in commands, free text, custom commands |
| **M5 — Release** | Installers, store listings, signing and provenance, docs; compact durable visit-ID index and migration |

M1 is a usable capture slice for development with no agent required. It is not a public download until the extension and signed installers complete M5.

**M0 as built (2026-09-27):** `protocol/schema.json` generates the shared Rust and TypeScript wire types. The Rust host implements framed native messages for discovery and configuration, checks the protocol version before decoding a version-specific request, and returns structured errors. Config updates require a revision from `get_config`; the host locks, rechecks, and atomically replaces a bounded config so stale panels cannot overwrite one another. The root-scoped vault API publishes new notes without replacing user files and reports cleanup or durability warnings after publication as a created file. Capture remains disabled until M1 adds site allowlisting and permission enforcement. The vault write API is internal and is not exposed as a native message in M0. CI is configured for macOS and Windows.

Before M1 exposes vault writes, the host must bind the notes root to a user selection made in a native OS folder picker. The current M0 `update_config` message alone does not prove user intent. M1 uses create-only page notes; an existing-file edit becomes a separate review artifact. Automatic replacement awaits a cross-platform displaced-byte backup design that survives an external-editor race.

### 12.1 M1 implementation and acceptance plan

1. **Prove setup authority before capture.** The settings page opens a persistent native connection and asks the host to show an OS folder picker. On macOS this uses a directory-only `NSOpenPanel`; on Windows the M1 host opens the Common Item Dialog without a parent window. Chrome supplies `--parent-window=0` when a service worker starts the host. Parenting to a nonzero Chrome window handle requires a separately audited adapter and Windows validation of handle lifetime and focus. Each synchronous picker or confirmation runs on the main thread of a short-lived child host process, which pauses the native connection until the dialog closes and then exits. A long-lived host that showed an AppKit modal and then blocked on stdin left macOS with a persistent busy cursor during the first smoke run; a fresh child also gives each Windows dialog its own COM apartment. The child only reports the user's choice; the parent validates the result and mints every token. The host canonicalizes the result and returns a short-lived, single-use selection token bound to the folder's filesystem identity. A config update changing or repairing `storage.root` must present that token and the current config revision; a raw path alone is rejected. Cancellation and stale tokens leave config unchanged. The host gives the panel a repair path for malformed, newer, or identity-less config files without performing capture. Exercise the picker and thread model on both OSes before M1 acceptance.
2. **Make consent and permission state explicit.** The MV3 manifest declares `incognito: "not_allowed"` and broad *optional* HTTP(S) match patterns, with no site origin granted at install. A setup button requests optional `webNavigation` and the chosen exact origin directly from the live Chrome user gesture, before awaiting the host; path-specific adapter rules remain host-enforced. The host shows its own native confirmation for every privacy-expanding config change and issues a single-use token bound to the exact change and revision. If that confirmation is canceled, remove the newly granted Chrome permissions and keep capture off. Only after host confirmation can the settings page enable capture. The service worker caches the host-confirmed policy with its revision and bounded expiry, fails closed if absent or expired, and awaits trusted-only `storage.local` access before buffering. Removing a grant or disabling a site stops capture immediately and clears its pending records. The host also disables a revoked site when informed, while acknowledging that it cannot inspect Chrome's live grant state. The service worker checks the incognito flag before buffering.
3. **Deliver visits across service-worker and panel lifecycles.** Capture only main-frame, active-document navigation and SPA URL changes for granted sites. The service worker stores a count- and byte-capped queue with stable event IDs and original URLs in `chrome.storage.local`, restricted to trusted extension contexts. It never relies on an in-memory dwell timer. On the next panel connection it sends pending records; the host authorizes the original URL against its allowlist and path rule before normalization. It returns a terminal per-event outcome: `persisted`, `suppressed`, or permanent-policy `rejected` with a reason. Transient vault/host errors (including an unavailable folder or failed durability step) return a retryable outcome; the event stays queued with bounded backoff and a visible error. The extension removes only terminally acknowledged records and reports rejections and overflow in the panel. Complete Markdown event markers are the persistence proof; the OS config intent locates the original log on replay.
4. **Write recoverable Markdown.** Normalize only HTTP(S) URLs, reject credentials, and drop fragments and configured tracking parameters without erasing meaningful query data after authorization. Site-specific transforms follow M1 as separately consented adapter rules. Under the config and capture locks, recheck the current host policy, sync an event intent outside the notes folder, append accepted visits with IDs to `log/YYYY/MM/YYYY-MM-DD.md`, and sync before acknowledging persistence. Retry completes only a byte-exact interrupted prefix in the currently selected root; it never overwrites unrelated user edits. The near-repeat window suppresses redundant entries without rewriting user text. Create page notes using a mandatory URL-derived identity suffix and record the canonical URL. Verify identity and ownership before treating an existing note as already present. Never replace an existing note in M1: write any proposed edit as a no-clobber sibling review artifact keyed by the proposed content, so a replay reuses the same draft. Malformed markers are conflicts. The UI must distinguish created, already present, conflict, and created-with-warning outcomes.
5. **Verify the complete no-agent path on both OSes.** CI covers normalization, permission and allowlist enforcement, incognito rejection, replay/idempotence including concurrent host processes, transient retry, traversal, and create-only page ownership/conflicts on macOS and Windows. A real Chrome smoke run with an unpacked development extension and development-only native-host registration checks first-run setup, panel close/reopen, visit replay, permission removal, picker cancellation, and page-note creation. No release claim or installer download link is made during M1.

The plan follows Chrome's current [optional-permission rules](https://developer.chrome.com/docs/extensions/reference/api/permissions), [incognito behavior](https://developer.chrome.com/docs/extensions/reference/manifest/incognito), [service-worker lifecycle](https://developer.chrome.com/docs/extensions/develop/concepts/service-workers/lifecycle), [storage access levels](https://developer.chrome.com/docs/extensions/reference/api/storage/StorageArea), and [native-messaging window handle](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging) contract. Folder picker details follow [Apple's `NSOpenPanel`](https://developer.apple.com/documentation/appkit/nsopenpanel) and [Microsoft's Common Item Dialog](https://learn.microsoft.com/en-us/windows/win32/shell/common-file-dialog).

**M1 validation remaining:** The macOS guided run loaded the unpacked extension, completed a Chrome-to-host `hello`, and exercised picker cancellation with config unchanged. The full permission, consent, visit, page-note, and removal flow was stopped before completion after a persistent macOS spinning cursor appeared; it is not a smoke-test pass. Native dialogs now run in a short-lived child process to remove that cause, which still needs confirming in a repeat macOS run; then run the flow on Windows. macOS and Windows CI build and test the branch. A failed append now rolls back its own bytes, and replay tolerates complete visits appended after an intent, so only a crash mid-append followed by further visits still requires manual repair. Confirm the Chrome permission request accepts explicit default and nondefault ports on both operating systems. Automated tests cover host identity/replay recovery and extension repair, pause, policy refresh, re-enable races, title capture, and notice retention. The M1 per-event intent journal grows indefinitely and needs a compact durable replacement before general distribution. Windows currently syncs file content but has no separately validated directory-entry flush, so power-loss durability of a newly created daily log or page-note name remains unverified. Resolve these gaps before the public package is released.

## 13. Platforms and License

- **Browsers:** Google Chrome Stable only; see the release support window in §11. Edge and all other browsers are out of scope for the first release.
- **Operating systems:** macOS 15 Sequoia and later on Intel or Apple silicon; Windows 11 version 25H2 and later supported releases on x64. Windows 10, Windows on ARM, and Linux are out of scope for the first release.
- **License:** Apache-2.0, chosen for its explicit patent grant.

## 14. Open Questions

No open M0 design questions. The release hosting, support, platform, browser, and signing policies are defined in §11; M1 security requirements are recorded in §12.
