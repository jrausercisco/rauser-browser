# Rauser Browser Browsing Assistant — Design Specification

> Status: Draft v0.5 · M0 merged, M1 accepted for development on macOS, no public package yet · Short name "Brauser"; CLI and binary `brauser` · License: Apache-2.0 · Platforms: Google Chrome on macOS and Windows

## 1. Purpose

Brauser turns the browser into a place where context accumulates instead of disappearing. It runs as an always-on sidebar that:

- **Logs** important sites the user visits
- **Summarizes** pages into a separate summary file per page, using the user's own local AI agent
- **Annotates** sites with persistent, per-page notes
- **Saves** pages for later
- **Surfaces** related pages from the user's history and notes
- Provides an **omnibar** for commands and free-text workflows, usually operating on the current page

Captured notes, browse logs, and read-later items are plain Markdown in a folder the user chooses. Operational config and a rebuildable search index stay in standard per-user OS locations. No cloud service, no account, no telemetry.

## 2. Design Principles

1. **The user owns the data.** Markdown files are the source of truth. Everything else, such as the search index, is a rebuildable cache.
2. **Least privilege everywhere.** The extension cannot touch the filesystem. The host cannot do anything it isn't configured to do. Sites are opt-in.
3. **Page content is untrusted.** Any page the user visits may contain hostile content, including prompt injection aimed at the agent.
4. **Never clobber user edits.** Brauser only modifies what it owns. Summary files are the one kind of file Brauser owns whole, and a new summary replaces the old one, including any edits to it (§5.2).
5. **Format-neutral, convention-friendly.** Output works in any markdown tool. Obsidian is a first-class preset, not a dependency.
6. **Configuration over code.** Agent harnesses, site adapters, and omnibar commands are defined in config, not hardcoded.
7. **Assume nothing about the user's setup.** No default vault location, agent, folder structure, or site list. Brauser ships with examples, and the user makes every choice explicitly at setup. It never writes into a folder the user hasn't chosen.
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
- Sidebar via the Chromium `sidePanel` API for the current page: its note, summary, related pages, and the omnibar, with capture status in a collapsible strip (§5.7). The side panel's header has a settings (gear) button, and the panel shows a warning with a link to settings until a notes folder, at least one site, and capture are configured.
- A dedicated settings page (`options_ui`, opened in a tab) holds configuration: notes folder, enabled sites, and pause. A full tab hosts Chrome's permission prompt and the native dialogs better than the narrow panel. The panel keeps a one-click pause as an emergency stop.
- Holds UI state only. It stores no secrets and makes no network requests to external services.
- No remote code: no `eval`, no dynamically loaded scripts, and a strict extension CSP. From M2 the manifest sets `content_security_policy.extension_pages` to `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'none'; media-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; form-action 'none'; base-uri 'none'`. Native messaging does not go through CSP.

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
| Prompt injection from page content | Content delimited and labelled untrusted; agent run in its most restricted mode for read-only tasks; outputs written only to Brauser-owned locations; output shown as text and stored with remote embeds neutralized (§5.2) |
| Sensitive pages sent to an agent | Agent denylist enforced in the host, overriding explicit user requests with a warning |
| Unwanted browsing surveillance | Logging is opt-in per domain; incognito never logged; a summary file is written only for a page the user explicitly summarizes, and is the only record Brauser keeps of pages on sites not enabled for logging |
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

- The host stores the selected folder's filesystem identity after the native picker. Each privileged operation opens a root-scoped directory handle and checks that identity; a moved or replaced folder requires reselection, including repair at the same path. The identity is the volume device and file ID, and a remounted external or network volume can get a new device number, so it may need reselection too. A missing or mismatched folder blocks capture and notes but keeps every other setting, including the site allowlist, for the repair; a folder that comes back with its old identity needs no repair.
- Filenames are generated by the host from normalized slugs. The extension never supplies a raw path.
- M0 creates new page files only: write a temp file in the same directory, fsync it, then publish with an atomic no-clobber hard link. It never replaces an existing file.
- Page notes are edited only in the side panel (§5.3), so the host replaces a note whole: write a temp file in the same directory, fsync it, then atomically rename it over the note. Each save names the note version it was based on; if the file has changed since then (for example, from another Brauser window), the host refuses the save and returns the current note. Before replacing, the host checks the file's recorded canonical URL and Brauser ownership, as for summaries (§5.2).
- M2 summary files (§5.2) are also replaced whole, without a version check. Brauser owns the whole file, so the `/summarize` action is the confirmation for replacing it. The host writes and fsyncs a temp file in the summaries directory, then publishes with a no-clobber hard link if the name is absent, or renames over the existing name only if it is a regular file (not a symlink) whose leading frontmatter has `brauser.kind: summary` and the requested `canonical_url` and `url_id`. Anything else is a `conflict` and is left untouched. On Windows the rename uses `MOVEFILE_REPLACE_EXISTING`; a sharing violation or access-denied error while another program holds the file is retried briefly, then returned as `retryable` with nothing changed. The window between the check and the rename is accepted for summary files only: an external save in that window loses only an edit to Brauser's summary text, never note text. Logs and read-later files stay create-only.
- Destructive operations, such as deleting a note or read-later file, require explicit user confirmation in the sidebar. Saving a note's text is not destructive.

### 4.5 Agent invocation rules

- Each harness is a config entry: absolute binary path, argument template, and environment allowlist.
- Spawned with a cleared environment plus allowlisted variables, a timeout, an output size cap, and cancellation support.
- Page content goes on stdin inside a delimited block with an explicit instruction that it is untrusted data.
- For summarization and Q&A, harnesses should be configured in a mode without shell or file-writing tools. The host writes results itself; the agent never writes into the vault directly.
- The user's agent denylist (§7.3) blocks content from those domains from ever reaching an agent. The host checks the tab's URL before the extension extracts content, then checks the URL the extractor returns from inside the document before accepting the content, and checks again before spawning the harness (§5.2). The check on the returned URL is the one that counts. These checks defend against navigation races and page-driven navigation; the extension remains untrusted (§4.1), and the host cannot verify the reported URL beyond this.

## 5. Features

### 5.1 Site logging

- Logging is **opt-in**: the user enables a site and grants its exact origin from a setup action. The host independently checks scheme, host, and any adapter path rule before recording a visit; Chrome's origin permission does not enforce URL paths.
- Incognito and private windows are never logged, regardless of config. The extension manifest uses `incognito: "not_allowed"`, and the event handler resolves the tab's incognito state and rejects it defensively.
- M1 observes main-frame `webNavigation.onCommitted` and `onHistoryStateUpdated` events, using document identity and lifecycle to avoid subframes, redirects, and duplicate SPA records. It does not keep dwell or scroll timers only in service-worker memory. A committed document's title arrives after commit, so the worker briefly holds a new untitled visit and fills its title from the tab's title update; the event is frozen once handed to the panel because the host hashes it for interrupted-write recovery. Each visit carries its local UTC offset, and the daily log uses that local calendar date.
- M1 normalization drops fragments and configured tracking parameters after host authorization. Later opt-in site adapters may apply site-specific transforms (for example, collapsing a GitHub file view to its repository).
- Deliveries are idempotent by event ID recorded in the Markdown log. The host keeps a durable per-event intent in its OS config directory to find the original daily log across date and `log_dir` changes; only a complete Markdown marker proves persistence. Shared locks cover the current policy check, intent, append, and file sync, so revocation and concurrent panel connections cannot race a write. An interrupted append can resume only from an exact byte prefix in the still-selected folder; changed or missing logs require review. Near-repeat visits within a configurable window are suppressed rather than rewriting a prior Markdown entry; the daily log remains append-only. Dwell-time and scroll-depth filters follow M1 only after their state can survive service-worker suspension.

### 5.2 Summaries

1. The extension extracts readable content (Readability-style) from a tab on demand. Any HTTP(S) page can be summarized. Without `tabs`, the side panel cannot see an ungranted tab's URL, and a side-panel click does not grant `activeTab` (§8). Summarizing therefore starts in one of two ways:
   - A trigger that grants `activeTab`: the `summarize` keyboard command, the page context menu's "Summarize with Brauser", or the toolbar action. The grant is temporary, needs no prompt, and covers only that tab until it navigates. The worker records the tab and document the grant was issued for, opens or focuses the panel synchronously in the handler (`sidePanel.open`, before any await), and the panel summarizes only that target.
   - The panel's Summarize button, when the extension already holds the page's exact origin or an `activeTab` grant for it. On any other page the button tells the user to use the shortcut, context menu, or toolbar action. It never asks for broad access, and it does not use the logging-only `webNavigation` permission to learn tab URLs.

   A trigger for summarizing does not enable logging; the host's site rules still decide what is logged.
2. The panel sends the tab's URL to the host, which answers `allowed` or `denied` from the denylist (§4.5). On `denied` nothing is extracted. Otherwise the extractor runs in the main frame only and returns `location.href`, `document.title`, and the content together. If the canonical form of the returned URL differs from the one checked, the panel discards the content and reports that the page changed. The host checks the returned URL against the denylist again, uses it as the only key for the summary file, and invokes the configured harness with the summary prompt.
3. Output streams back to the sidebar in chunks, staying under the 1 MB native messaging limit per message. Granularity depends on the harness (§7.1). Agent output is untrusted: the panel renders it only as text (`textContent`), with no Markdown-to-HTML conversion, so it loads nothing and never navigates by itself. Streamed text is provisional; a `rejected` run (§7.1) clears it.
4. The host holds no config or capture lock while the harness runs. It snapshots the config revision and harness entry at spawn. Before writing, it takes the config lock and a per-page summary lock, re-reads config, and rechecks the notes-folder identity, the summaries location, the denylist for both the original and the returned URL, and that the harness entry still matches. Any failure discards the output and nothing is written. The per-page lock serializes concurrent summaries of one page; the last to finish replaces the file. Canceled, timed-out, over-cap, rejected, non-zero-exit, and panel-closed (EOF) runs write nothing.
5. The host writes the final summary itself, to a summary file that is a separate record from the page note (§6.1). The note holds only the user's own text; Brauser never writes a summary into it. The host writes a summary only from a harness run it started itself for that URL; no message lets the extension supply summary text. A summary may be written for any HTTP(S) URL that passes normalization and the denylist, whether or not its site is enabled for logging, and it never creates a page note, log entry, or visit intent. Each page has one summary file, `<summaries_dir>/<slug>.summary.md`, keyed by the same URL-derived identity as its page note; the fixed suffix means no profile filename template can make it equal a note name. Its host-written frontmatter records `brauser.kind: summary`, the canonical URL, and `url_id` (§6.6), and its first line links to the page and to the page note's computed path in the profile's link style. The note link resolves once the note exists; changing `pages_dir` or `summaries_dir` leaves it stale until the page is summarized again. Summarizing again replaces the whole file (§4.4). Summary files belong to Brauser, and user edits to them are replaced. A file without a matching summary identity is a conflict and is never overwritten. The ownership check reads only the leading host-written frontmatter, so agent text cannot change a file's identity.
6. Before writing, the host neutralizes every construct that could make a Markdown viewer fetch a remote resource: inline images, reference-style images with their link definitions, and raw HTML tags such as `<img>`, `<iframe>`, `<video>`, `<audio>`, `<object>`, `<embed>`, `<link>`, and `<style>`. Each becomes an inert code span with its URL still visible. Tests cover each construct in Obsidian and VS Code preview.
7. The final message is `summary_result`, correlated by `request_id`, sent after the write is attempted. Its `outcome` is `created`, `replaced`, `created_with_warning`, `replaced_with_warning` (the file is in place but temp cleanup or directory sync failed, as with page notes; not a failure and not retried), `conflict` (names the occupying file and says to move or delete it), or `retryable` (a transient vault error before publication), with `relative_path` and `message`. After `conflict` or `retryable`, the host keeps the final text in memory, keyed by `request_id`, until the write succeeds, the panel discards it, or the connection closes; Retry sends only the `request_id` and does not rerun the harness. The panel keeps the text on screen with Copy, Retry save, and Discard; if the connection is lost it becomes a copy-only unsaved view. A canceled, timed-out, or failed run shows its partial text marked unsaved.

### 5.3 Page notes

- A page note is a persistent scratch space attached to the page. The user can add to it at any time; when they return to the page, the panel shows it again.
- One markdown file per normalized URL. It is created on the first keystroke, not when a page is opened, so visiting a page never creates an empty note.
- Notes work on any HTTP(S) page, not only logged sites. The panel cannot see an ungranted tab's URL (§5.2), so the first note on a site starts, like summarizing, from the toolbar action, a keyboard command, or the page context menu, which grant `activeTab`; the panel then requests that exact origin from Chrome so it can read the tab's URL and title on later visits. A grant made for a note does not enable logging.
- The note's title comes from the page's title when the note is created (or its host name when the page has none); the panel asks only for the note text.
- The panel autosaves: it sends the whole note about one second after the user stops typing, and again when the active tab changes or the panel closes. There is no Save button. A small indicator shows "Saving…", "Saved", or an error.
- The side panel is the only supported editor. The files stay plain Markdown and can be read anywhere, but an edit made outside Brauser can be overwritten. The version check in §4.4 exists so two Brauser windows do not overwrite each other: when a save is refused, the panel loads the newer note and shows the text it could not save so the user can copy it back.
- A page note holds only the user's thoughts. Agent output never goes into a note; it goes to the page's separate summary file (§5.2) or wherever a command sends it (§5.6).
- Notes created by M1 load into the editor normally. M1 review drafts are left in place and are not merged.

### 5.4 Read later

- `/save` writes a read-later entry. It may link to the page's summary file (§5.2); the summary itself is never copied into the entry.
- Items can be marked done, which moves them in the index but never deletes the file.

### 5.5 Related pages

- The host maintains a SQLite FTS5 index over titles, summaries, notes, and tags.
- When the active tab changes, the sidebar shows related pages ranked by text relevance, shared tags, and shared domain.
- v1 is keyword search. Embedding-based similarity is a later, optional stage.
- The index lives in the OS cache directory, not in the user's notes folder, and can be deleted and rebuilt at any time with `brauser reindex`.
- On each connect, the host does an incremental scan (by modification time) to pick up files edited outside Brauser. While connected, it watches the notes folder for changes. There is no scheduled reindexing.

### 5.6 Omnibar

Input is either a slash command or free text.

| Command | Action |
|---|---|
| `/summarize` | Summarize the page into its summary file |
| `/note <text>` | Add text to the end of the page note |
| `/save` | Add the page to read later |
| `/related` | Show related pages |
| `/log` | Log the current page now, on any HTTP(S) page, even if its site is not set up for logging |
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
output = "sidebar"          # sidebar | new_note | clipboard
```

No command writes agent output into a page note (§5.3), and only `/summarize` writes a page's summary file (§5.2). `new_note` creates a new Brauser-owned file and never replaces an existing one. To keep agent text in a page note, the user copies it from the sidebar. Config loading rejects `output = "page_note"`.

Template variables: `{page.title}`, `{page.url}`, `{page.content}`, `{selection}`, `{input}`, `{date}`.

A command's output never goes into the page note (§5.3). `new_note` writes a separate Brauser-owned file.

`/log` and notes are one-off actions the user takes on a single page, so they need Chrome's grant for that site but not the host's native confirmation (§7). `/log` writes one entry to the daily log; it does not enable automatic logging for the site.

### 5.7 Side panel layout

The panel is about the page in the active tab. Capture housekeeping collapses into a one-line strip at the bottom, and configuration stays on the settings page.

```
┌────────────────────────────────────────────┐
│ Rauser Browser Browsing Assistant       ⚙  │  gear opens settings
├────────────────────────────────────────────┤
│ Designing Data-Intensive Apps — Ch. 5      │  current page; follows the
│ oreilly.com/library/view/…/ch05            │  active tab
│ ● Logged   ✦ AI allowed        ☆ Save      │  status chips; ☆ = /save
├────────────────────────────────────────────┤
│ [ This page ]   Read later (3)             │
├────────────────────────────────────────────┤
│ ┌ ANSWER ─────────────────────── ✕ ──────┐ │  only after a question or a
│ │ Leader-based replication sends every   │ │  sidebar-output command;
│ │ write through one node…▌               │ │  streams, not saved
│ │ [Stop]                         [Copy]  │ │
│ └────────────────────────────────────────┘ │
│ SUMMARY                     2 days ago  ↻  │  §5.2, its own file
│ Replication keeps copies of the same data  │
│ on several machines…   Show more · Open ↗  │
│ MY NOTE                          Saved ✓   │  §5.3, autosaves
│ ┌────────────────────────────────────────┐ │
│ │ This is how our orders DB fails over.  │ │
│ └────────────────────────────────────────┘ │
│ RELATED                                    │  §5.5
│ • Kafka: The Definitive Guide — ch. 6      │
│ • Orders DB failover (your note)           │
├────────────────────────────────────────────┤
│ ┌────────────────────────────────────────┐ │  omnibar (§5.6); typing /
│ │ Ask about this page, or type /     ⏎   │ │  opens the command list
│ └────────────────────────────────────────┘ │
│ ● Capture on · 0 pending               ▸   │  expands to Send now,
└────────────────────────────────────────────┘  Discard…, Dismiss, Pause
```

The Read later tab lists saved pages with a search box and a checkbox to mark each done; done items move to a collapsed "Done" group and their files are kept.

Each state changes one area rather than the layout:

| State | Panel |
|---|---|
| Not set up | The setup warning replaces the tabs; the omnibar and page controls are disabled; the strip reads "Off". |
| Native host unavailable | A banner under the header with the fix; everything below is disabled. |
| Site not logged | Chip `○ Not logged · Log this site…`, which opens settings with the site filled in. Notes, `/log`, and summaries still work. |
| Site on the agent denylist | Chip `⊘ AI off for this site`; summarize and questions are disabled with the reason. |
| No agent set up | Chip `✦ AI not set up`; the omnibar offers only commands that need no agent, plus a link to settings. |
| Not an HTTP(S) page | The page area names the tab and says Brauser can't use it; only Read later is active. |

Milestones fill the layout in: the page area, My note, and the capture strip first; the answer card, Summary, and omnibar in M2; Related and Read later in M3; the full command list in M4.

## 6. Data Format

### 6.1 Folder layout

The user chooses the notes folder at setup. It can be empty, or a subfolder of an existing knowledge base. The four content locations are configurable; the layout below is the suggested starting point offered at setup, not a requirement.

```
<notes folder>/                  chosen by the user, no default
  log/2026/09/2026-09-27.md      daily browse log
  pages/<slug>.md                page note: the user's text only
  summaries/<slug>.summary.md    one Brauser-owned summary per page (M2)
  later/<slug>.md                read-later items
```

No content location may equal, contain, or sit inside another, compared case-insensitively; the host rejects an overlapping config. An M1 config has no `summaries_dir`: capture keeps working, `/summarize` reports `not_configured`, and the first harness setup proposes `summaries` in its native confirmation (§7.3). An overlap found then is a recoverable setup error, never resolved silently.

Brauser's own files live outside the notes folder, in standard OS locations, so it never adds hidden folders to someone's knowledge base:

| File | Location |
|---|---|
| `config.toml` | OS config directory (e.g. `~/Library/Application Support/Brauser` on macOS, `%APPDATA%\Brauser` on Windows) |
| `visit-ids/` | OS config directory; M1 keeps one synced intent file per persisted visit |
| `index.db` | OS cache directory |

If Brauser finds an existing folder with files it didn't create, it leaves them alone. It only indexes files under its configured content locations.

### 6.2 Internal model

The host works on two record types. A `PageNote` has `url`, `canonical_url`, `url_id`, `title`, `created`, `updated`, `tags`, `links`, and `body`. A `Summary` has `url`, `canonical_url`, `url_id`, `title`, `generated`, `harness`, `harness_version`, `note_link`, and `body`; it is Brauser-owned and replaced whole (§5.2). The index joins them by `url_id`. Profiles only control serialization, including how `note_link` is rendered.

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
summaries_dir = "summaries"          # M2; proposed at the first harness setup
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

**Reading is lenient, writing follows the profile.** The indexer parses both link styles and any frontmatter format. Switching profiles never rewrites existing files; `brauser migrate --profile <name>` converts them and defaults to a dry run.

### 6.4 Managed blocks

In page notes, Brauser owns only:

- Its own frontmatter keys, namespaced under `brauser:`
- Content between `<!-- brauser:start -->` and `<!-- brauser:end -->`

Page notes no longer use managed blocks: the side panel is their only editor, so the host writes the whole file (§4.4, §5.3), and summaries live in their own files (§5.2). Managed blocks remain the rule for any later feature that writes into a file the user also edits elsewhere. Such replacement is deferred until a cross-platform atomic displaced-byte backup and conflict procedure is proven with an external-editor race test.

Page-note filenames include a stable identifier derived from the normalized canonical URL, even when a profile uses a title slug. Before updating a file, the host verifies the recorded canonical URL and Brauser ownership match the requested page. An absent or different identity is a conflict, never an opportunity to adopt or overwrite an unrelated file. Records carry `brauser.kind`: `summary` for summary files; a page note without a `kind` key, as all M1 notes are, is a `page`. The page-note path rejects a file whose kind is `summary`, and the summary path accepts only exactly one `kind: summary` line.

### 6.5 Example page note (neutral profile)

```markdown
---
title: Designing Data-Intensive Applications — Chapter 5
url: https://example.com/ddia/ch5
tags: [replication, databases]
brauser:
  canonical_url: https://example.com/ddia/ch5
  url_id: 65fc5f8734006f2e6ac748dc5daadc458ac24dc3fc98c2233160c601c42389a7
  created: 2026-09-27T10:14:00Z
  updated: 2026-09-27T10:15:12Z
  visits: 3
---

Compare this with how our event pipeline handles failover.
Check whether the orders DB uses sync or async replicas.
```

### 6.6 Example summary file (neutral profile, M2)

```markdown
---
title: Designing Data-Intensive Applications — Chapter 5
url: https://example.com/ddia/ch5
brauser:
  kind: summary
  canonical_url: https://example.com/ddia/ch5
  url_id: 65fc5f8734006f2e6ac748dc5daadc458ac24dc3fc98c2233160c601c42389a7
  generated: 2026-10-02T09:41:07Z
  harness: claude-code
  harness_version: 2.1.283
---

Summary of [the page](https://example.com/ddia/ch5). Note: [Designing Data-Intensive Applications — Chapter 5](../pages/example-com-ddia-ch5-65fc5f8734006f2e6ac748dc5daadc458ac24dc3fc98c2233160c601c42389a7.md)

Leader-based replication trades write availability for consistency...
```

Under the neutral profile the note link is a relative Markdown link computed from `summaries_dir` to `pages_dir`, with path segments percent-encoded; under the obsidian preset it is a wikilink.

## 7. Configuration

Config lives in the OS config directory (§6.1) and is owned by the host. The extension reads and edits it only through host messages, which validate every change and require the revision returned by `get_config`. In M1, a change to `storage.root` requires a short-lived, single-use token from a native folder picker. `update_config` must reject an arbitrary root path supplied by the extension. Adding or widening a site matcher, enabling capture, or weakening a privacy exclusion also requires a native host confirmation and a short-lived, single-use token bound to the exact config delta and current revision. Narrowing or disabling capture can take effect immediately. One-off actions the user takes on a single page, such as writing a note or running `/log`, are not config changes and need no native confirmation: the click or keystroke is the consent, and Chrome's site grant still applies. A Chrome grant is a separate browser access gate and does not authorize a host config change. Canceling a picker or confirmation leaves config unchanged. A malformed or newer on-disk config must produce a recoverable setup error rather than preventing the host from responding at all.

The config starts nearly empty. Setup writes only what the user chooses; everything else is either off or an inert, commented-out example. The host refuses to perform an action whose required config is missing and tells the extension what to configure, rather than guessing. Configuration is edited on the extension's settings page; the side panel links to it.

### 7.1 Agent harnesses

An agent is optional. Without one, logging, notes, read later, and related pages all still work; only AI commands are disabled.

Brauser ships adapter templates for Claude Code, Codex, and a generic CLI, but configures none of them automatically. At setup, the host may look for known harnesses on `PATH` and offer what it finds; the user confirms the binary path and chooses one. Nothing is enabled without that confirmation.

```toml
[agent]
default = "claude-code"              # whichever the user chose; unset means no agent

[agent.harnesses.claude-code]
binary = "/usr/local/bin/claude"
args = ["-p", "{prompt}", "--tools", "", "--disallowedTools", "*",
        "--strict-mcp-config", "--setting-sources", "", "--permission-mode", "dontAsk",
        "--no-session-persistence", "--output-format", "stream-json",
        "--verbose", "--include-partial-messages"]
env_allow = ["HOME", "PATH"]
timeout_secs = 120

[agent.harnesses.codex]
binary = "/usr/local/bin/codex"
args = ["exec", "--sandbox", "read-only", "--ignore-user-config", "--ignore-rules",
        "--strict-config", "--ephemeral", "--skip-git-repo-check", "--json",
        "-c", "approval_policy=\"never\"", "-c", "web_search=\"disabled\"",
        "-c", "project_doc_max_bytes=0",
        "--disable", "shell_tool", "--disable", "unified_exec", "--disable", "apps",
        "--disable", "plugins", "--disable", "multi_agent", "--disable", "hooks",
        "--disable", "memories", "--disable", "browser_use", "--disable", "computer_use",
        "--disable", "image_generation", "{prompt}"]
env_allow = ["HOME", "PATH"]
timeout_secs = 120

[agent.harnesses.generic]
binary = "/path/to/any-cli"
args = ["{prompt}"]
stdin = "page"
env_allow = ["HOME", "PATH"]
timeout_secs = 120
```

`{prompt}` is the command prompt only. Page content always goes on stdin, never into argv. `env_allow` is required in every harness; a missing one means an empty environment. `timeout_secs` defaults to 120.

The argument lists above follow the Claude Code 2.1.283 and codex-cli 0.144.4 documentation and help output (checked 2026-09-28). Trial runs of both harnesses the same day informed the rules below; the templates have not yet passed M2 acceptance. Each harness starts in a host-owned empty working directory, emptied before each run, so no project instructions or project config load and Claude Code does not leave a new per-directory entry under `~/.claude/projects` on every run. `--max-turns` is omitted: with no tools there is no second turn, and it is not listed in `--help`.

**Authentication and environment.** A Chrome-spawned host does not inherit the login shell's environment, and `--setting-sources ""` skips the `env` block in `~/.claude/settings.json`, so Bedrock, Vertex, or API-key users need their variables allowlisted. Setup finds out how the harness authenticates and proposes the needed names (for example `CLAUDE_CODE_USE_BEDROCK`, `AWS_PROFILE`, `AWS_REGION`, `ANTHROPIC_API_KEY`); the native harness confirmation shows names, never values. Setup then runs a one-token probe with the full argv and refuses the harness if it fails.

**Isolation.** `HOME` is passed only so the harness can find its credentials, but it also exposes user-level instructions, settings, hooks, MCP servers, plugins, and memory, so isolation must be proven, not assumed. Claude Code with `--tools ""` and `--disallowedTools "*"` has no tools; if `--setting-sources ""` is rejected, `--safe-mode` also stops CLAUDE.md, hooks, MCP servers, and plugins from loading. Administrator-managed settings still apply. The host parses Claude Code's `system`/`init` event and kills the process tree if `tools`, `mcp_servers`, or `plugins` is non-empty. Claude Code reads stdin to EOF before emitting it, so this limits what a misconfigured run can do and return; it does not keep page content from the harness. Codex still loads the user's global `AGENTS.md` and user skills even with `--ignore-user-config`, so the host runs it with `HOME` and `CODEX_HOME` set to host-owned directories holding only the credential the user approved at setup (§14, Open M2 questions). M2 acceptance includes a canary run for each template against a scratch `HOME` containing real credentials plus a user instruction file saying "reply CANARY", a hook that writes a marker file, a user MCP server, a user skill, and a memory entry; a template passes only if none has any effect.

**Tool use.** Codex documents no switch that removes every tool, and its feature names change between versions. The host accepts a Codex run only while every event is on an allowlist recorded for the harness version: thread and turn lifecycle, usage, errors, and `item.*` events whose `item.type` is `agent_message` or `reasoning`. At the first other event, known or unknown, the host kills the process tree, sends the panel a `rejected` message that clears any streamed text, and writes nothing. A chunk is forwarded only after its event passes the allowlist. For Claude Code, any `tool_use` block does the same. This is detection, not prevention: a Codex tool can read local files before its event arrives. The read-only sandbox is what prevents writes, and disabling `shell_tool` and `unified_exec` is what prevents commands. Setup therefore also passes `--disable` for every feature `codex features list` reports enabled and not needed for text output (at 0.144.4 this includes `in_app_browser`, `browser_use_external`, `browser_use_full_cdp_access`, `code_mode_host`, `goals`, `remote_plugin`, `plugin_sharing`, and `tool_suggest`), and refuses a Codex version that reports an enabled feature not on the reviewed list. Claude Code, which runs with no tools, is the recommended harness for page content.

**Checks and results.** At setup, and again before any run where the binary's identity (resolved real path, size, and modification time) has changed, the host runs `--version`, confirms in `--help` (and `codex features list`) every flag it will pass, and records the results. Both harnesses update themselves in place, so this re-check matters. A passing re-check continues without asking the user again; a failure, or a real path outside the confirmed install location, disables AI commands and the settings page shows why. The host never falls back to other flags. A run succeeds only when the process exits 0 and, for Claude Code, the final `result` event has `is_error: false` (an auth failure can report subtype `success` with `is_error: true`); error text is never shown as summary content. stderr is kept for diagnostics only, since Codex writes ERROR lines on successful runs. Streaming granularity differs: Claude Code's `stream-json` emits token deltas, while `codex exec --json` 0.144.4 emits only whole items, so the panel shows progress until the `agent_message` arrives. Cancellation kills the process tree either way.

Prompt injection in page content can still distort a summary or try to smuggle page data into URLs. With Claude Code it cannot run commands or write files. With Codex it cannot write files or run shell commands, and any other tool use aborts the run, though a tool may already have read a file. Neither the panel nor a stored summary loads remote resources (§3.1, §5.2).

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
agent_denylist_confirmed = false     # set only by the host through native confirmation
strip_params = ["utm_*", "fbclid", "gclid"]
```

The denylist starts empty rather than shipping a guess at what the user considers sensitive. The first harness setup shows the denylist and offers suggested categories (banking, email, HR, health) as editable example hostnames; none is added unless the user chooses it. No AI command runs until the user confirms the list once, even if they leave it empty.

The host enforces that gate. The first harness setup's native confirmation shows the exact denylist, with an explicit "no domains excluded" line when it is empty, next to the harness binary, arguments, environment names, and proposed `summaries_dir`. Its single-use token is bound to the config revision, the harness entry, and that exact denylist, and commits `agent_denylist_confirmed = true` in the same write; a changed denylist or revision invalidates it. `update_config` rejects the key in every other case. A missing key (any config written before M2) counts as false, and the host refuses agent requests with a setup error pointing to settings. Removing or replacing a harness does not reset it. Adding entries takes effect immediately; removing entries weakens a privacy exclusion and needs native confirmation (§7).

Each entry is a hostname. The host stores it lowercased, converted to punycode, and without a trailing dot, and rejects entries with a scheme, path, port, or wildcard. An entry matches that host and every subdomain on any scheme and port; IP literals match exactly. Checks use the original URL before normalization.

## 8. Extension Permissions

| Permission | Why |
|---|---|
| `sidePanel` | Always-on sidebar |
| `nativeMessaging` | Talk to the host |
| `storage` | UI preferences and the capped visit buffer (§3.2) |
| `webNavigation` | Optional in M1; requested only when the user enables logging |
| `scripting` | Required, M2; runs the pinned content extractor in a tab under an `activeTab` grant or an enabled site's grant. Adds no install warning without host permissions |
| `activeTab` | Required, brought forward from M2 by the side-panel rework (§12.2 step 4); grants temporary access to one tab from the `note` command, the page context menu, or the toolbar action. No install warning |
| `contextMenus` | Required, brought forward from M2 by the side-panel rework; the page context menu's "Add a note in Brauser" (M2 adds "Summarize with Brauser" alongside it) |
| `commands` | A `note` entry, brought forward from M2, starts the first note on an ungranted page (§5.3); `summarize` remains M2 |
| `optional_host_permissions` | Manifest declares HTTP(S) patterns for sites discovered during setup; Chrome grants only the exact origin requested for an enabled logging site or a first note. Adapter paths remain host-enforced |

The manifest's broad optional HTTP(S) patterns permit runtime requests for user-chosen sites; no origin is granted at install time. Logging permission requests run directly from a settings-page button while Chrome still has the user gesture, before awaiting native confirmation. If native confirmation is canceled, the extension removes any newly granted origin and optional API permission. Chrome remembers granted optional permissions after the extension removes them, so a later request for the same origin can succeed without a new Chrome prompt (§12.1); the host's native confirmation, not Chrome's prompt, is the consent record for logging. Removing a grant stops capture for that origin and purges its buffered events. The service worker reads the title from tab metadata on granted sites; M1 requested neither `scripting` nor `activeTab`. The side-panel rework (§12.2 step 4) added `activeTab`, `contextMenus`, and a `note` command ahead of M2, and with them raised `minimum_chrome_version` from 114 to 116 (`sidePanel.open` from a command or context-menu handler needs Chrome 116). `scripting` is still M2-only.

M2 adds `scripting` and a `summarize` entry under `commands`, reusing the `activeTab` and `contextMenus` permissions the side-panel rework already added. A side-panel click grants neither `activeTab` nor the tab's URL, so summarizing an ungranted page starts from the `summarize` command, the context menu, or the toolbar action (§5.2, §5.6), the same pattern notes and `/log` already use for the first note or log entry on a new site (§5.3). Notes and `/log` request the page's exact origin; summarizing does not. M2 does not add `tabs`, which Chrome labels "Read your browsing history", and makes no persistent origin grants for summarizing, so none accumulate and logging-site removal never affects summarizing.

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
- **Round-trip tests:** every profile writes, reads back, and re-writes page notes and summary files with no diff, and user content outside managed blocks survives unchanged.
- **CI on every PR:** `clippy -D warnings`, `rustfmt`, `cargo-deny`, ESLint, TypeScript strict, dependency review, and the full test suite on macOS and Windows.

## 11. Distribution

- **Download experience:** provide one public Brauser download page with two clearly labeled steps: install the browser extension from the Chrome Web Store, then download and run the signed native-host installer for the user's OS. The page explains why both parts are needed, links to supported-browser instructions, and offers a short troubleshooting path. The native host is a companion installer; do not ask users to build from source, use a terminal, or install Homebrew to get started.
- **Extension:** publish the stable release through the Chrome Web Store. Keep the published extension ID stable because the native-host manifest authorizes that exact ID. Chrome Web Store installation is the supported path on Windows and macOS; do not instruct users to install a local CRX.
- **Supported browser:** Google Chrome Stable only, on macOS and Windows. Test the current and previous two stable major versions and publish the tested range on the download page. Edge, Brave, Arc, other Chromium browsers, Firefox, and Safari are unsupported in the first release. Chrome's Side Panel API requires Chrome 114 or later, and M2's `sidePanel.open` requires 116; the release floor must also satisfy the current three-version support window.
- **macOS package:** offer a signed and notarized per-user `.pkg` for macOS 15 Sequoia and later, with a universal Intel and Apple silicon host binary and a Chrome user-level native-messaging manifest. Sign the app with the publisher's Developer ID Application identity and the installer package with its Developer ID Installer identity. Homebrew may be offered as an additional channel for technical users, not as the primary consumer install path.
- **Windows package:** offer a signed per-user x64 installer for Windows 11 version 25H2 and later supported releases. Register the native host under `HKEY_CURRENT_USER` for Google Chrome only, without administrator rights. Sign the installer and binaries with a trusted Authenticode code-signing certificate and timestamp the signatures. Windows 10 and Windows on ARM are unsupported in the first release. Document what happens when organizational policy blocks per-user installation.
- **Native-host registration:** register only Google Chrome, using its documented user-level manifest location on macOS and registry entry on Windows. The installer must use the stable Chrome Web Store extension ID in `allowed_origins`; it must never use wildcard origins. `brauser register` may repair the Chrome registration after installation, with a clear confirmation and status report. Uninstallation removes only Brauser's host registration and installed binaries.
- **First-run flow:** after the extension is installed, opening the side panel checks for the host and gives a direct OS-specific download link if it is missing. After host installation, the panel verifies host/protocol compatibility, warns that setup is incomplete, and links to the settings page, which guides the user through choosing a notes folder, profile, optional agent, optional sites, and privacy exclusions. Explain permissions at the point they are requested. Show a final review of what will be captured and where it will be written; capture stays off until the user finishes setup. Include a no-agent setup path that reaches a usable capture experience.
- **Upgrade and removal:** browser-store updates apply to the extension; host updates use the signed OS installer and preserve the configured notes folder. Show a clear incompatibility error if extension and host protocol versions do not match. Uninstall leaves the user's notes and config untouched by default, and the documentation explains how to remove them manually.
- **Release page and support:** use the public GitHub repository's Releases page as the canonical download page. Each release links to the Chrome Web Store listing, the macOS `.pkg`, Windows x64 installer, checksums, install/upgrade/rollback/uninstall instructions, troubleshooting, and the GitHub Issues support channel. Do not publish a release until both OS installers and the extension have passed the install and uninstall checks.
- **Release artifacts:** publish signed installers, checksums, SLSA build provenance, and an SBOM for every release. Reproducible builds are a stretch goal.

## 12. Build Order

| Milestone | Scope |
|---|---|
| **M0 — Foundation** | Protocol schema, host skeleton, config loading, vault confinement, atomic writes, CI |
| **M1 — Capture** | Side panel and settings page, site logging, URL normalization, daily log, create-only page notes with review drafts |
| **M2 — Agent** | Harness setup with native confirmation, content extraction, streaming with cancellation, `/summarize`, denylist enforcement |
| **M3 — Related** | FTS5 index, related pages, read later, `reindex` |
| **M4 — Omnibar** | Built-in commands, free text, custom commands |
| **M5 — Release** | Installers, store listings, signing and provenance, docs; compact durable visit-ID index and migration; Windows smoke run and directory-entry durability |

M1 is a usable capture slice for development with no agent required. It is not a public download until the extension and signed installers complete M5.

**M0 as built (2026-09-27):** `protocol/schema.json` generates the shared Rust and TypeScript wire types. The Rust host implements framed native messages for discovery and configuration, checks the protocol version before decoding a version-specific request, and returns structured errors. Config updates require a revision from `get_config`; the host locks, rechecks, and atomically replaces a bounded config so stale panels cannot overwrite one another. The root-scoped vault API publishes new notes without replacing user files and reports cleanup or durability warnings after publication as a created file. Capture remains disabled until M1 adds site allowlisting and permission enforcement. The vault write API is internal and is not exposed as a native message in M0. CI is configured for macOS and Windows.

Before M1 exposes vault writes, the host must bind the notes root to a user selection made in a native OS folder picker. The current M0 `update_config` message alone does not prove user intent. M1 uses create-only page notes; an existing-file edit becomes a separate review artifact. The side panel rework (§12.2) replaces this with the versioned note replace in §4.4, which treats the side panel as the only editor; Brauser-owned summary files (§5.2) use the checked rename there.

### 12.1 M1 implementation and acceptance plan

1. **Prove setup authority before capture.** The settings page opens a persistent native connection and asks the host to show an OS folder picker. On macOS this uses a directory-only `NSOpenPanel`; on Windows the M1 host opens the Common Item Dialog without a parent window. Chrome supplies `--parent-window=0` when a service worker starts the host. Parenting to a nonzero Chrome window handle requires a separately audited adapter and Windows validation of handle lifetime and focus. Each synchronous picker or confirmation runs on the main thread of a short-lived child host process, which pauses the native connection until the dialog closes and then exits. A long-lived host that showed an AppKit modal and then blocked on stdin left macOS with a persistent busy cursor during the first smoke run; a fresh child also gives each Windows dialog its own COM apartment. The child only reports the user's choice; the parent validates the result and mints every token. The host canonicalizes the result and returns a short-lived, single-use selection token bound to the folder's filesystem identity. A config update changing or repairing `storage.root` must present that token and the current config revision; a raw path alone is rejected. Cancellation and stale tokens leave config unchanged. The host gives the panel a repair path for malformed, newer, or identity-less config files without performing capture. Exercise the picker and thread model on both OSes before M1 acceptance.
2. **Make consent and permission state explicit.** The MV3 manifest declares `incognito: "not_allowed"` and broad *optional* HTTP(S) match patterns, with no site origin granted at install. A setup button requests optional `webNavigation` and the chosen exact origin directly from the live Chrome user gesture, before awaiting the host; path-specific adapter rules remain host-enforced. The host shows its own native confirmation for every privacy-expanding config change and issues a single-use token bound to the exact change and revision. If that confirmation is canceled, remove the newly granted Chrome permissions and keep capture off. Only after host confirmation can the settings page enable capture. The service worker caches the host-confirmed policy with its revision and bounded expiry, fails closed if absent or expired, and awaits trusted-only `storage.local` access before buffering. Removing a grant or disabling a site stops capture immediately and clears its pending records. The host also disables a revoked site when informed, while acknowledging that it cannot inspect Chrome's live grant state. The service worker checks the incognito flag before buffering.
3. **Deliver visits across service-worker and panel lifecycles.** Capture only main-frame, active-document navigation and SPA URL changes for granted sites. The service worker stores a count- and byte-capped queue with stable event IDs and original URLs in `chrome.storage.local`, restricted to trusted extension contexts. It never relies on an in-memory dwell timer. On the next panel connection it sends pending records; the host authorizes the original URL against its allowlist and path rule before normalization. It returns a terminal per-event outcome: `persisted`, `suppressed`, or permanent-policy `rejected` with a reason. Transient vault/host errors (including an unavailable folder or failed durability step) return a retryable outcome; the event stays queued with bounded backoff and a visible error. The extension removes only terminally acknowledged records and reports rejections and overflow in the panel. Complete Markdown event markers are the persistence proof; the OS config intent locates the original log on replay.
4. **Write recoverable Markdown.** Normalize only HTTP(S) URLs, reject credentials, and drop fragments and configured tracking parameters without erasing meaningful query data after authorization. Site-specific transforms follow M1 as separately consented adapter rules. Under the config and capture locks, recheck the current host policy, sync an event intent outside the notes folder, append accepted visits with IDs to `log/YYYY/MM/YYYY-MM-DD.md`, and sync before acknowledging persistence. Retry completes only a byte-exact interrupted prefix in the currently selected root; it never overwrites unrelated user edits. The near-repeat window suppresses redundant entries without rewriting user text. Create page notes using a mandatory URL-derived identity suffix and record the canonical URL. Verify identity and ownership before treating an existing note as already present. Never replace an existing note in M1: write any proposed edit as a no-clobber sibling review artifact keyed by the proposed content, so a replay reuses the same draft. Malformed markers are conflicts. The UI must distinguish created, already present, conflict, and created-with-warning outcomes.
5. **Verify the complete no-agent path on both OSes.** CI covers normalization, permission and allowlist enforcement, incognito rejection, replay/idempotence including concurrent host processes, transient retry, traversal, and create-only page ownership/conflicts on macOS and Windows. A real Chrome smoke run with an unpacked development extension and development-only native-host registration checks first-run setup, panel close/reopen, visit replay, permission removal, picker cancellation, and page-note creation. No release claim or installer download link is made during M1.

The plan follows Chrome's current [optional-permission rules](https://developer.chrome.com/docs/extensions/reference/api/permissions), [incognito behavior](https://developer.chrome.com/docs/extensions/reference/manifest/incognito), [service-worker lifecycle](https://developer.chrome.com/docs/extensions/develop/concepts/service-workers/lifecycle), [storage access levels](https://developer.chrome.com/docs/extensions/reference/api/storage/StorageArea), and [native-messaging window handle](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging) contract. Folder picker details follow [Apple's `NSOpenPanel`](https://developer.apple.com/documentation/appkit/nsopenpanel) and [Microsoft's Common Item Dialog](https://learn.microsoft.com/en-us/windows/win32/shell/common-file-dialog).

**M1 validation status** (summarizing the dated entries below): The first macOS guided run was stopped by a persistent busy cursor after a native dialog. Native dialogs now run in a short-lived child process, and the repeat runs below, including the unattended `--auto` run, passed the full flow on macOS on nondefault and default ports. The Windows interactive run and its default-port check are parked and are release blockers under M5 (§12.2). macOS and Windows CI build and test every change. A failed append rolls back its own bytes, and replay tolerates complete visits appended after an intent, so only a crash mid-append followed by further visits still requires manual repair. Automated tests cover host identity/replay recovery and extension repair, pause, policy refresh, re-enable races, title capture, and notice retention. The M1 per-event intent journal grows indefinitely and needs a compact durable replacement before general distribution. Windows has no separately validated directory-entry flush, so power-loss durability of a newly created daily log or page-note name remains unverified. These gaps must be resolved before the public release.

**macOS smoke pass (2026-09-28):** `npm run smoke:macos` completed the full flow on macOS with Chrome 154: first-run warning, picker cancellation, declined consent (with the new Chrome grants rolled back), confirmed setup, allowed and blocked visit replay, panel reopen without duplication, page note creation, identical retry, one review draft and its retry, site removal with Chrome grants removed, and no capture after removal. No busy cursor persisted after any native dialog, which confirms the child-process dialog fix. The fixture's nondefault port was accepted by Chrome's permission request. Re-requesting access that Brauser had just removed did not show a second Chrome prompt, so the host's native confirmation, not Chrome's prompt, is the gate for that re-enable; the host still required it. The runner now drives the extension pages itself through the DevTools pipe and stops only for the picker, Chrome's prompt, and the host dialog.

**macOS default port and unattended run (2026-09-28):** `node scripts/smoke-macos.mjs --default-port --auto` passed the full flow with the fixture on port 80 and the site entered as `http://127.0.0.1:80`. Chrome granted exactly `http://127.0.0.1:80/*` and not the any-port pattern, and the host stored the origin without the port. A repeat run on a nondefault port also passed. `--auto` answers every prompt through macOS UI scripting, so the run needs no person once the terminal has Accessibility access. The host's folder picker does not answer Accessibility queries and is driven by keyboard. Its Yes/No alert is drawn by `UserNotificationCenter`, and Chrome's permission prompt is a sheet on the settings window, so both buttons are pressed by name. In place of the busy-cursor question, the runner checks that no dialog process or host window remains. **M1 is accepted for development on macOS.**

**Page note title (2026-09-28):** The panel's page-note form no longer has a title field. The host receives the tab's title, or the host name when the page has none; a change in that title still produces a review draft.

**Settings page (2026-09-28):** Configuration moved from the side panel to a dedicated `options_ui` page. The panel shows a gear button and a setup warning until a folder, a site, and capture are all in place. The page opened from the panel on macOS once Brauser was reloaded in `chrome://extensions`. Chrome keeps an unpacked extension's manifest and service worker from load time but serves rebuilt pages from disk, so a build that changes `manifest.json` or `worker.ts` needs that reload; the panel now says so when the settings page cannot open. `npm run smoke:macos` starts a fresh profile and is unaffected. This is not a smoke-test pass for the flow above.

### 12.2 Next steps

1. **M1 acceptance on macOS: done (2026-09-28).** The full run passed on both a nondefault and the default port (above).
   - **Windows parked (2026-09-28).** No Windows machine is available, so the Windows smoke runner and its interactive run are deferred. Windows CI continues to build and test every PR. Porting the runner (Chrome path, native-host registration in the registry rather than the profile, and no `pbcopy`), the Windows run, and its default-port check are release blockers tracked under M5.
2. **Catch page-load regressions in CI (optional, recommended).** Unit tests import the pages but do not load the built extension. A headless Chrome step that loads `extension/dist/` and opens the panel and settings page would have caught the stale-manifest class of failure before a manual run.
3. **Settle M2 design before code: done (2026-09-28).** The answers are in §14 and in §4.4, §5.2, §6, §7.1, §7.3, and §8. One open question remains in §14; it must be answered before the Codex template ships and does not block the Claude Code path.
4. **Rework the side panel and make notes editable (before M2): done (2026-09-28).** Built the §5.7 layout with the controls M1 already has: the page area following the active tab, the autosaving note (§5.3) with the host's versioned replace (§4.4), notes on any HTTP(S) page started from the toolbar action, a keyboard command, or the context menu (§5.3), which brings `activeTab`, `contextMenus`, and a `note` entry under `commands` (§8) forward from M2, and the capture strip collapsed into a one-line disclosure. This replaces the create-only note flow and review drafts. Protocol version 3 replaces `create_page_note`/`page_note_result` with `load_note` (returns `exists`, a `revision`, `title`, `body`) and `save_note` (takes `expected_revision`; a match replaces the note whole and returns `note_saved`, a stale revision returns `note_conflict` with the note's current content instead of writing anything). The host reuses `get_config`/`update_config`'s revision-check pattern; before replacing, it checks the existing file's recorded canonical URL and `brauser:` ownership (kind absent or `page`, never `summary`), never adopting a mismatch. The answer card, Summary section, and full omnibar remain M2/M3 and are not built; the omnibar's one-line input and the `/summarize`, `/related`, and `Read later` surfaces have no placeholder yet beyond the capture strip. The headless smoke run (`npm run smoke:macos:headless`) now exercises autosave (typing, then the debounce save; a save flushed by navigating the page away and back) and the refused-save path (a simulated second window saves first, so the panel's own save is refused, shows the current note, and keeps the unsaved text in a copy-out box; a later edit then saves cleanly against the now-current revision) in place of the removed create-only/review-draft checks. Not yet verified against real Chrome: whether the toolbar action alone (via `openPanelOnActionClick`, with no `action.onClicked` handler) grants `activeTab` the way the `note` command and context-menu item do — the same open question M2's build order already flags for `summarize` (§12.2 step 5.4).
5. **Build M2 in this order**, each step reviewed and merged separately:
   1. Harness setup: the host offers harnesses found on `PATH` and shows a native confirmation of the binary, arguments, environment variable names, the denylist review (§7.3), and the proposed `summaries_dir` (§6.1). As with `storage.root`, the extension never supplies a raw binary path; `update_config` requires a single-use token bound to the confirmed harness entry and denylist, and that token sets `agent_denylist_confirmed`. The same setup records the harness version and identity, checks every flag against `--help` (and `codex features list`), runs the one-token probe, and refuses a harness that fails (§7.1). Add optional `summaries_dir` and the privacy keys to the protocol `StorageConfig`/config schema, validation, and overlap checks, with a protocol version bump. Tests cover the host refusing agent requests while the denylist is unconfirmed and `update_config` rejecting an extension attempt to set it.
   2. Host invocation: an environment of only the allowlisted variables, argv from the template, page content on stdin inside the untrusted-data block, the host-owned working directory, and a timeout, output cap, and process-tree kill. Test with fake harness binaries on both OSes, including one whose symlink target changes between runs (identity re-check), one that emits a tool call or an unknown Codex event, and one that reports `is_error`. Run the §7.1 canary for each shipped template, including a prompt-injected page that asks Codex to read a planted file; it passes if the contents appear in no forwarded chunk or summary and any non-allowlisted event ends the run as `rejected`.
   3. Streaming protocol: the host currently answers one request at a time, so it needs a reader that stays responsive while a harness runs. Add chunk, `rejected`, error, `cancel`, and `summary_result` messages correlated by `request_id`, each under the 1 MB native-messaging limit (§5.2). Closing the panel (stdin EOF) kills the harness.
   4. Content extraction with a bundled, pinned Readability-style library, the `summarize` command, context menu, and toolbar trigger, and the permissions in §8. First confirm with the real-Chrome smoke runner that (a) the command, context-menu, and toolbar-action paths grant `activeTab` to `chrome.scripting.executeScript`, and whether opening the panel from the action (`openPanelOnActionClick`) grants it; (b) the grant ends when the tab navigates; (c) the panel sees no URL for an ungranted tab; (d) the grant does not carry over to another tab in the same window.
   5. `/summarize` in the panel, with denylist checks and the summary write and result contract in §5.2 and §4.4. Test an external-editor race, a Windows rename while another process holds the file open, a shared or overlapping summaries directory, and a config change during a fake-harness run, which must discard the output.
6. **M3–M5** follow as in the table. The compact visit-ID index, the Windows smoke run, and Windows directory-entry durability are release blockers tracked under M5.

## 13. Platforms and License

- **Browsers:** Google Chrome Stable only; see the release support window in §11. Edge and all other browsers are out of scope for the first release.
- **Operating systems:** macOS 15 Sequoia and later on Intel or Apple silicon; Windows 11 version 25H2 and later supported releases on x64. Windows 10, Windows on ARM, and Linux are out of scope for the first release.
- **License:** Apache-2.0, chosen for its explicit patent grant.

## 14. Open Questions

No open M0 or M1 design questions. The release hosting, support, platform, browser, and signing policies are defined in §11; M1 security requirements are recorded in §12.

M2 decisions (2026-09-28):

- **Extraction permissions.** Any page can be summarized. A side-panel click does not grant `activeTab`: Chrome's documentation lists only the action click, context menus, keyboard commands, and the omnibox, and Chromium withholds tab permissions from the side panel. Without `tabs`, the panel also cannot see an ungranted tab's URL, so it has no origin to request. Summarizing an ungranted page therefore starts from a trigger that grants `activeTab`: the `summarize` keyboard command, the page context menu, or the toolbar action. The panel's button works when the origin is already granted or an `activeTab` grant is present. M2 adds `activeTab` and `contextMenus` and does not add `tabs` (§5.2, §8). This refines the earlier "prompt only if needed" answer: no Chrome prompt is needed, and no persistent grant is made for summarizing.
- **Summary destination.** A summary is a separate record from the page note: one Brauser-owned summary file per page, `brauser.kind: summary`, in its own non-overlapping `summaries_dir`, replaced whole on each new summary (user edits to it are replaced), linking to the page and its note (§5.2). The note holds only the user's own text. Because summaries never touch notes, M2 does not need the managed-block replacement in §6.4; the one replace primitive it needs, a checked rename limited to summary files, is in §4.4.
- **Harness modes.** Checked against the Claude Code 2.1.283 and codex-cli 0.144.4 documentation, help output, and trial runs (§7.1). Claude Code can run with no tools and is the recommended harness. Codex can run with its shell, web search, and user config disabled in a read-only sandbox, but documents no switch that removes every tool, so the host accepts only an allowlist of Codex event types and aborts on anything else.
- **Denylist setup.** Required once: the first harness setup's native confirmation shows the agent denylist with its suggested categories, and the host refuses every AI command until that confirmation sets `agent_denylist_confirmed`, even if the list is empty (§7.3).

Side panel decisions (2026-09-28):

- **Editable notes.** A page note is a scratch space the user can add to at any time, edited only in the side panel and autosaved as they type (§5.3). This replaces M1's create-only notes and review drafts. The host replaces the note whole and refuses a save based on an old version (§4.4).
- **No AI output in notes.** Commands send output to the sidebar, a new separate file, or the clipboard (§5.6). The `page_note` output is removed.
- **Notes on any page.** Notes work on any HTTP(S) page; the first note on a new site starts from the toolbar action, a keyboard command, or the context menu, because the panel cannot see an ungranted tab's URL; the panel then requests the site's origin from Chrome (§5.3).
- **`/log` on any page.** `/log` logs the current page now, even on a site not set up for logging, without enabling automatic logging there (§5.6).
- **No native confirmation for one-off actions.** Writing a note and running `/log` need only Chrome's site grant (§7). The trade-off: a compromised extension could write a note or log entry for any single page, where before it could do so only on sites the user confirmed. It still cannot enable automatic logging, change the notes folder, or weaken privacy settings without the host's confirmation.

Open M2 questions:

- **Codex isolation.** Running Codex with a host-owned `CODEX_HOME` keeps the user's global `AGENTS.md` and skills out, but needs a proof run showing token refresh does not break or corrupt the user's own Codex login. If it cannot be made safe, is it acceptable to ship the Codex template with user-level Codex instructions shaping Brauser's output, documented as such, or should Codex be dropped from M2?
