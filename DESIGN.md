# Rauser Browser Browsing Assistant — Design Specification

> Status: Draft v0.6 · M0 merged, M1 accepted for development on macOS, side-panel rework and M2 step 1 (harness setup) merged, artifact model (M1.5a, then M1.5b) next, no public package yet · Short name "Brauser"; CLI and binary `brauser` · License: Apache-2.0 · Platforms: Google Chrome on macOS and Windows
>
> Companion specification: [adapter-templates.md](adapter-templates.md), the adapter-template language artifact adapters are written in (§7.2), built in §12.2 step 6.2.

## 1. Purpose

Brauser turns the browser into a place where context accumulates instead of disappearing. It runs as an always-on sidebar that:

- **Remembers the work artifacts** the user opens (tickets, docs, pull requests, designs) as one record per artifact, with how long they focused on it and how they got there, and shows them in one Recent list and an optional worklog across every tool (§5.1)
- **Summarizes** pages into a separate summary file per page, using the user's own local AI agent
- **Annotates** sites with persistent, per-page notes
- **Saves** pages for later
- **Surfaces** related pages from the user's history and notes
- Provides an **omnibar** for commands and free-text workflows, usually operating on the current page

Captured notes, the artifact log, artifact files, and read-later items are plain Markdown in a folder the user chooses. Operational config and a rebuildable search index stay in standard per-user OS locations. No cloud service, no account, no telemetry.

## 2. Design Principles

1. **The user owns the data.** Markdown files are the source of truth. Everything else, such as the search index, is a rebuildable cache.
2. **Least privilege everywhere.** The extension cannot touch the filesystem. The host cannot do anything it isn't configured to do. Sites are opt-in.
3. **Page content is untrusted.** Any page the user visits may contain hostile content, including prompt injection aimed at the agent.
4. **Never clobber user edits.** Brauser only modifies what it owns. Brauser owns a few kinds of file whole: summary files (§5.2), artifact files and optional worklog files regenerated from the log (§5.1, §6.7), and page notes, which are edited only in the side panel (§5.3). Any edit made elsewhere to a summary, artifact, or worklog file is replaced when that file is next written. Brauser never adopts or replaces a file whose recorded kind and identity do not match.
5. **Format-neutral, convention-friendly.** Output works in any markdown tool. Obsidian is a first-class preset, not a dependency.
6. **Configuration over code.** Agent harnesses, site adapters, and omnibar commands are defined in config, not hardcoded.
7. **Assume nothing about the user's setup.** No default vault location, agent, folder structure, or site list. Brauser ships with examples, and the user makes every choice explicitly at setup. It never writes into a folder the user hasn't chosen.
8. **No always-running native service.** The host runs only while the side panel or settings page is open. If the user enables artifact logging, the browser's event-driven extension service worker may wake on navigation and focus changes while the browser is open to buffer visits and focus intervals on enabled origins; nothing runs when the browser is closed.

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
- Sidebar via the Chromium `sidePanel` API for the current page: its note, summary, related pages, and the omnibar, with capture status in a collapsible strip (§5.7). The side panel's header has a settings (gear) button, and the panel shows a warning with a link to settings until a notes folder, at least one enabled artifact adapter, and capture are configured.
- A dedicated settings page (`options_ui`, opened in a tab) holds configuration: notes folder, enabled artifact adapters and their origins, and pause. A full tab hosts Chrome's permission prompt and the native dialogs better than the narrow panel. The panel keeps a one-click pause as an emergency stop.
- Holds UI state only. It stores no secrets and makes no network requests to external services.
- No remote code: no `eval`, no dynamically loaded scripts, and a strict extension CSP. From M2 the manifest sets `content_security_policy.extension_pages` to `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'none'; media-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; form-action 'none'; base-uri 'none'`. Native messaging does not go through CSP.

### 3.2 Native host

- One Rust host binary per supported OS and CPU architecture, with `#![forbid(unsafe_code)]` in all first-party crates except `macos-alert`, a small macOS-only wrapper around `CFUserNotification` that lets the Yes/No confirmation time out and be canceled.
- The only component with filesystem and process privileges.
- Treats every inbound message as untrusted, because a compromised page or extension context could influence it.
- Registered through a native messaging manifest whose `allowed_origins` lists only the official extension IDs.

**Lifecycle.** The host runs only while the side panel or settings page is open. It is never a daemon, service, or login item. Each of those pages opens its own native messaging connection when it loads, the browser spawns a host process for it, and that host exits when the connection closes (stdin EOF), which happens when the page closes. Work the host would otherwise do in the background, such as catching up on external edits to notes, happens on connect instead (see §5.5).

While the panel is closed, the extension's service worker still observes navigation on enabled origins and buffers qualifying events in `chrome.storage.local` (with artifact logging, hourly `activity` events; see below). Each record has a stable event ID, original HTTP(S) URL, optional title, and timestamp. The buffer is capped by count and bytes. Before writing any record, the worker awaits `setAccessLevel({accessLevel: "TRUSTED_CONTEXTS"})`, loads a revisioned, host-confirmed site policy with a bounded expiry, and checks the current Chrome grant; absent or expired policy fails closed. The host independently enforces its confirmed site allowlist and privacy rules against the original URL before any lossy normalization. The host cannot inspect Chrome's live grants. Events leave the buffer only after a terminal host acknowledgement (`persisted`, `suppressed`, or permanent-policy `rejected`); transient errors retain the event for bounded retry and appear in the panel alongside rejections and queue overflow. An invalid or newer host config suspends the extension policy without deleting buffered visits; the panel offers an explicitly confirmed discard action during repair. Disabling logging or confirming a site grant revocation purges affected records. The side panel and settings page serialize config changes and Chrome grant cleanup across every open window through one shared lock, and recheck the grant before enabling capture. The worker accepts messages only from those two extension pages. Each page watches the worker's policy lease and re-reads the host when another page publishes a different revision. An explicit re-enable clears a prior revocation only when the worker has the committed host revision and Chrome currently grants that origin. Summaries and other agent tasks run only while the panel is open.

With artifact logging (§5.1), recognition has to happen while the page is live, because the host is usually not running when a visit happens. The host-confirmed policy lease the worker already caches therefore also carries each enabled adapter's confirmed adapter templates (match rules, `id` and title templates, mode map, and scope; §7.2) and, from M1.5b, its Tier 1 selectors, under the same revision and expiry. The worker prefilters with them: a visit no adapter matches is dropped before it reaches the buffer, and a matching visit is buffered with its provisional `artifact_id` and, when the adapter has Tier 1 fields, the values read from the live page. The prefilter only narrows capture. At delivery the host re-runs its own recognition on the original URL and title and discards any field the confirmed adapter does not declare, so a compromised worker can drop visits but cannot widen what is recorded. An absent or expired lease fails closed, as in M1. The worker also accumulates attention into one pending total per artifact per local hour in trusted-only `storage.local`. The total stays pending until its hour ends; the worker then freezes it into one `activity` event in the buffer (§5.1).

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
| Unwanted browsing surveillance | Automatic logging is opt-in per adapter and origin. Only artifact activity reaches the delivery buffer: the worker drops other pages on an enabled origin before buffering, using the confirmed rules in its policy lease, and the host re-checks every delivered event (§3.2, §5.1). The one exception is the Tier 2 hint's sample in the worker: at most 20 URL shapes per enabled origin, with IDs and title-like text replaced by placeholders and no titles, kept for 7 days and never written to the notes folder. It records what kinds of page the origin has, not which pages were visited (§5.1). A compromised worker could still buffer or deliver anything it sees on a granted origin, but the host records only what a confirmed adapter matches. Incognito is never logged. On pages outside enabled origins, Brauser keeps only what the user explicitly creates: a page note, a `/log` entry, or a summary |
| Hostile adapter templates or captured fields | Adapters use adapter templates, not regular expressions: matching is linear in its capped input in both engines by construction, with no backtracking, and both engines are checked against shared vectors, differential tests, and fuzzing (§7.2). No capture type admits `/`, `:`, `%`, whitespace, or control characters, so captures cannot form paths or ambiguous IDs (the host builds slugs). Titles and DOM fields are untrusted text, escaped and embed-neutralized before writing (§7.2) |
| Personal activity recorded on shared origins | On origins that host many tenants (GitHub, Google Docs, Notion, Figma, Linear), an adapter must restrict captures to listed tenants with `scope`, or carry an explicit `unscoped` acknowledgment whose native confirmation says it records every account in the Chrome profile and recommends a separate work profile (§7.2) |
| A page choosing which note or summary it gets | The host derives every record key from the URL through confirmed Tier 0 rules and aliases only; the extension never sends a key, and Tier 1 fields and page content never affect one (§6.2) |
| Page-controlled text steering a drafted adapter | The sample holds only URL shapes, never titles, and a shape keeps only short lowercase route words, so a page has almost no text of its own to plant (§5.1). Drafts are still untrusted, limited to Tier 0, and written in adapter templates, which have no free-form patterns; the host rejects drafts that break the draft structure rules, overlap an existing adapter, or match the origin root, search pages, or probe URLs, and shows a match preview of every sampled shape and probe URL before the native confirmation (§5.1) |
| Confidential titles leaving the machine through a synced notes folder | Setup names what artifact logging records and warns when the folder is inside a known sync location (§5.1) |
| Other local extensions talking to the host | `allowed_origins` restricted to official extension IDs |
| Supply chain compromise | Pinned dependencies, `cargo-audit`/`cargo-deny`, npm lockfile audit, signed releases with provenance |

### 4.2 Threats out of scope

- A compromised local user account or OS
- A compromised agent harness binary
- Malicious config files written by the user

### 4.3 Host message handling

- Messages are length-prefixed JSON. M0 caps extension-to-host bodies at 4 MiB and host-to-extension bodies at 900 KiB, below Chrome's 1 MiB outbound limit. A response that would exceed the outbound cap is replaced by a correlated `message_too_large` error; the host keeps serving. Page-note bodies are capped at 98304 characters (counted as UTF-16 units, as the panel's textarea counts them) and 384 KiB, and a save that could not be returned whole by a later load is refused, so a saved note always loads; a note grown past either limit outside the panel loads as `message_too_large`, never into a textarea that could not accept further typing. A new note's title is clamped to 2048 bytes, not refused.
- Deserialized into typed structs with `deny_unknown_fields`. Unknown message types are rejected.
- Every message carries a protocol version and request ID. Version mismatch returns a structured error.
- Parser and path handling are fuzzed continuously (see §10).

### 4.4 Filesystem rules

- The host stores the selected folder's filesystem identity after the native picker. Each privileged operation opens a root-scoped directory handle and checks that identity; a moved or replaced folder requires reselection, including repair at the same path. The identity is the volume device and file ID, and a remounted external or network volume can get a new device number, so it may need reselection too. A missing or mismatched folder blocks capture and notes but keeps every other setting, including the site allowlist, for the repair; a folder that comes back with its old identity needs no repair.
- Filenames are generated by the host from normalized slugs. The extension never supplies a raw path.
- M0 creates new page files only: write a temp file in the same directory, fsync it, then publish with an atomic no-clobber hard link. It never replaces an existing file.
- Page notes are edited only in the side panel (§5.3), so the host replaces a note whole: write a temp file in the same directory, fsync it, then atomically rename it over the note. Each save names the note version it was based on; if the file has changed since then (for example, from another Brauser window), the host refuses the save and returns the current note. Before replacing, the host checks the file's recorded record key (§6.2) and Brauser ownership, as for summaries (§5.2).
- M2 summary files (§5.2) are also replaced whole, without a version check. Brauser owns the whole file, so the `/summarize` action is the confirmation for replacing it. The host writes and fsyncs a temp file in the summaries directory, then publishes with a no-clobber hard link if the name is absent, or renames over the existing name only if it is a regular file (not a symlink) whose leading frontmatter has `brauser.kind: summary` and the requested record key (§6.2). Anything else is a `conflict` and is left untouched. On Windows the rename uses `MOVEFILE_REPLACE_EXISTING`; a sharing violation or access-denied error while another program holds the file is retried briefly, then returned as `retryable` with nothing changed. The window between the check and the rename is accepted for summary files only: an external save in that window loses only an edit to Brauser's summary text, never note text. Logs and read-later files stay create-only.
- Artifact files (§5.1, §6.7) are regenerated from the log and replaced whole with the same checked rename as summary files, matched on `brauser.kind: artifact` and the requested `artifact_id`. Anything else at the path is a `conflict` and is left untouched. Regeneration is lazy (§6.7), and a file whose new bytes equal its current bytes is not rewritten, so sync clients and version control see a change only when the content changed.
- Destructive operations, such as deleting a note or read-later file, require explicit user confirmation in the sidebar. Saving a note's text is not destructive.

### 4.5 Agent invocation rules

- Each harness is a config entry: absolute binary path, argument template, and environment allowlist.
- Spawned with a cleared environment plus allowlisted variables, a timeout, an output size cap, and cancellation support.
- Page content goes on stdin inside a delimited block with an explicit instruction that it is untrusted data.
- For summarization and Q&A, harnesses should be configured in a mode without shell or file-writing tools. The host writes results itself; the agent never writes into the vault directly.
- The user's agent denylist (§7.3) blocks content from those domains from ever reaching an agent. The host checks the tab's URL before the extension extracts content, then checks the URL the extractor returns from inside the document before accepting the content, and checks again before spawning the harness (§5.2). The check on the returned URL is the one that counts. These checks defend against navigation races and page-driven navigation; the extension remains untrusted (§4.1), and the host cannot verify the reported URL beyond this.

## 5. Features

### 5.1 Artifact logging

Brauser remembers the tickets and docs the user works on. It does not keep a list of visited URLs. The unit of history is the **artifact**, such as a Jira issue, a Confluence page, a Google Doc, or a GitHub pull request, identified by a canonical ID and not by a URL. Six URLs for the same Jira issue (the issue page, a board modal, a link with tracking parameters) resolve to one record. Pages that are not artifacts, such as search results, dashboards, and board views, are never logged.

Artifact logging replaces M1's per-site automatic logging; the two do not run side by side. M1's consent machinery stays: the exact-origin Chrome grant, the host's native confirmation, the policy lease, the buffer, event intents, and the append-only daily log. What changes is what qualifies for the log. A visit is recorded only when an enabled adapter (§7.2) recognizes its URL.

**Recognition.** An adapter maps a URL on an enabled origin to an artifact type and canonical ID, for example "this URL is Jira issue `PROJ-123` in `acme.atlassian.net`." Adapters are config (§7.2) and are recognized in three tiers:

- **Tier 0: URL and tab title.** This tier needs no permissions beyond the exact-origin grant, and it provides most of the value. Work tools put their identity in the URL and most of their metadata in the tab title, which the worker already reads from tab metadata on granted origins. Jira uses `/browse/PROJ-123`, or `?selectedIssue=PROJ-123` when the issue opens as a board modal. Confluence uses `/wiki/spaces/<space>/pages/<id>`. Google Docs, Sheets, and Slides use `/<kind>/d/<id>/`, with a title such as "Name - Google Docs". A GitHub pull request title carries the title, author, number, and repository. Notion, Figma, and Linear use stable path IDs. SharePoint often puts identity in a `sourcedoc` GUID in the query string. Every adapter must work at Tier 0.
- **Tier 1: declarative DOM fields (M1.5b).** Optional per adapter, for data the title does not carry, such as status, assignee, labels, or parent epic. The adapter lists named CSS selectors. On an already-granted origin, the extension runs one fixed, bundled extraction function through `scripting` (§8), with the selectors passed as arguments. It returns only those named fields, reads only each element's rendered `innerText`, and never reads form values or attributes. Values are capped plain text and are untrusted like any page content. The worker runs Tier 1 while the page is live, whether or not the panel is open, and only on a URL its lease-based prefilter has recognized (§3.2). It reads the fields once the tab's title update settles, which for an SPA route is after `onHistoryStateUpdated`, and retries once a few seconds later if a field is empty, because work tools often render late. The values travel in the buffered visit. At delivery the host keeps only the fields the confirmed adapter declares, capped again. A field the worker could not read stays empty; the host never fetches it later.
- **Tier 2: drafted adapters.** When the user keeps visiting unrecognized pages on an enabled origin, the panel says so, for example "Looks like you have recurring pages on `wiki.internal`." On request, the configured agent receives that origin's **URL shapes** and drafts an adapter. It never receives page content, titles, or real URLs.

  A shape is a URL path and query with every identifier and every piece of title-like text replaced by a typed placeholder, for example `/wiki/spaces/{slug}/pages/{num}/{slug}` or `/jira/software/projects/{key}/boards/{num}?selectedIssue={key}`. The worker builds the shape from the original URL before storing anything:
  - A path segment stays literal only if it is at most 20 ASCII lowercase letters with at most one `-` or `_`, such as `wiki`, `browse`, `pull`, or `merge_requests`. These are the route words an adapter matches on.
  - Every other segment becomes a placeholder chosen by what it looks like: `{num}` for digits only, `{key}` for an issue-style key (`[A-Z][A-Z0-9]+-\d+`), `{uuid}` for a UUID, `{id}` for anything else containing a digit (hex, base64-style document IDs), and `{slug}` for the rest (title-derived text such as `Release-blocker-SBOM` or a percent-encoded page name). The class, not the value, is what a draft needs to choose a capture type (adapter-templates.md §7): `{num}` suggests `int`, `{key}` suggests `jira_key`, `{uuid}` suggests `uuid`, and `{id}` suggests `hex` or `token`.
  - Query parameter names follow the same literal rule (a name that fails it becomes `{param}`), and every value becomes a placeholder of the same classes. The fragment is dropped.
  - The tab title is never stored. Drafts therefore carry no title template, and the whole tab title is used (§7.2) until the user adds one by hand.

  Shapes are enough to draft an adapter, because an adapter matches on route words and ID positions, and they keep no record of what the user actually read. They also remove the main prompt-injection path: before, a planted page title could steer the agent toward a draft that would log every page on the origin. A shape leaves a page only short lowercase route words to plant, but those are still untrusted. Drafts are written in adapter templates (§7.2), which can be reviewed by reading and whose structure the host can check mechanically, so the host still treats a draft as untrusted and checks it before anything is shown:
  - The shapes go to the agent inside the same delimited untrusted-data block as page content (§4.5), and the agent denylist (§7.3) applies to the origin. The instruction to the agent includes the adapter-template grammar, and the host parses the draft with the same parser as any config; a draft that does not parse is discarded without being shown.
  - A draft is limited to Tier 0 `match`, `id`, `modes`, and `refs`. A draft that uses `title`, `scope`, `unscoped`, `alias`, `fields`, or `path_case` is rejected; Tier 1 fields and `meta` aliases read the DOM, and scope decides whose activity is recorded. The user can add them by hand afterward, under the normal confirmation. On a shared origin, the host copies the scope of the origin's existing adapter into the draft, and rejects a draft on a shared origin that has no scoped adapter.
  - A draft may use only origins that are already enabled and cannot add an origin.
  - Structure rules replace the old regex inspection (adapter-templates.md §10.4): every rule has at least one literal segment before its first capture, `{*}` may not directly follow the first segment, and the ID uses at least one capture whose type is not an alternation. The host also rejects a draft that overlaps an existing adapter on its origin, or that matches the origin root or any of a fixed set of probe URLs the host builds for that origin: search and filter shapes (`/search?q=x`, `/?q=x`, and paths or query keys such as `search`, `query`, `q`, `jql`, and `filter`), and `/login`, `/home`, and `/dashboard`. The probe list is host-owned config.
  - The host also rejects a draft that takes an ID from a query parameter whose name is on its search-and-filter list (`q`, `query`, `search`, `jql`, `filter`, and others in the probe config). This catches drafts that turn search parameters into IDs.

  Before the native confirmation, the host shows a **match preview**. For every sampled shape, including the ones that are clearly not artifacts, and every probe URL, it shows whether the draft matches and the ID it would produce. The host tests a shape by filling each placeholder with a fixed example value of its class (`{num}` as `123456`, `{key}` as `ABC-123`, `{slug}` as `example-page`, and so on). It also shows how many of the origin's already-recognized artifacts the draft would re-map. Enabling the draft then takes the native confirmation that any widening of capture needs (§7), and that confirmation repeats the count of sampled shapes the draft matches. Unrecognized visits never reach the host, so the hint's sample lives in the worker. For each enabled origin it keeps at most 20 distinct shapes, each with a visit count and the date last seen, in trusted-only `storage.local`, separate from the delivery buffer. Shapes expire 7 days after they were last seen and are purged when the origin is disabled. They go to the host only when the user asks for a draft, and they never enter the notes folder. The sample is on by default, because it records only what kinds of page an origin has; the settings page shows it and can clear it. Tier 2 needs M2's harness invocation.

**Engagement.** Brauser records what it measured, not what it infers. It does not label an artifact "worked on"; it records "focused 42 min". The worker measures attention only on tabs whose current document its prefilter recognized, and keeps two separate numbers:

- **Focused:** the tab is the active tab in the focused Chrome window.
- **Background:** the tab is still the active tab of the last-focused window, but focus has left Chrome, for example to take notes in a terminal beside the doc. Brauser cannot tell whether the tab is still visible, so this time is kept separate and never added to focused time.

Either measure stops when the user switches tabs (`tabs.onActivated`), navigates in that tab, or closes it (`tabs.onRemoved`), or when `idle.onStateChanged` reports idle or locked. Focus moving between Chrome and another app (`windows.onFocusChanged`) moves time between the two measures and does not stop it. None of these events needs the `tabs` permission. The worker sets the idle detection interval to 5 minutes (configurable; Chrome's minimum is 15 seconds) instead of the 60-second default, because reading a long document involves no keyboard or mouse input. On `idle`, the time up to the idle report counts, so up to one detection interval of reading without input is counted. On `locked`, counting stops at the lock. Searches and filters use the measured values with visible thresholds, such as "focused at least 5 min", so no time range is left unclassified.

**Opens.** An open is a cold open: a navigation that makes a tab show an artifact it was not showing before. That means a main-frame commit, or an SPA route change (`onHistoryStateUpdated`) to a different artifact ID, from a link, a typed URL, a bookmark, a link opened in a new tab, or back or forward from another page. The worker counts one when the tab-to-artifact map (Trails, below) changes that tab's entry to this artifact.

Tab switches are not opens and are not logged at all. Switching to a tab that already shows the artifact (`tabs.onActivated`), Chrome regaining focus, and returning from idle or lock only start and stop the focused and background measures.

A commit with transition type `reload` is never an open, even when the map has no entry for the tab. Chrome uses that type for the user's own reload, for the reload when the user switches to a tab Chrome discarded to save memory, and for tabs restored with a session.

A route change within the same artifact only adds a mode; examples are a pull request's `/files` tab and a Google Doc going from `/view` to `/edit`. The same artifact opened in two tabs is two opens.

The panel says "focused 42 min · 15 min in background · opened 3 times". An hour or day with focused time but no open, such as a tab left open from the day before, shows only the time.

**Coarse log.** The log gets one line per artifact per active hour. Without this, every change to the focused and background measures would be its own line. That would be dozens a day for one ticket, because every tab switch starts or stops a measure.

The worker keeps one pending **activity** total per artifact per local hour. The total is keyed by provisional `artifact_id`, local date, hour, and UTC offset, and is kept in trusted-only `storage.local` so it survives a browser restart. It holds:

- the open count (Opens, above);
- focused and background seconds;
- the modes seen;
- the latest canonical URL, title, and Tier 1 values;
- the distinct trail sources (`from` and transition), capped at 10.

A tab switch changes only the seconds; nothing records that the switch happened. When the hour ends, the worker freezes the total into one `activity` event with a stable event ID. Delivery sends only frozen events. The panel's live view reads the current hour's pending total from the worker.

A heavy workday then produces at most one line per artifact per hour. The cost is that the log knows only the hour in which something happened, and a measure still running when the browser exits is lost. The host appends a delivery batch as one write, one sync, and one intent record covering every event ID in it, not one durable intent file per event.

**Trails.** Each visit also carries how the user got there. `webNavigation.onCommitted` gives the transition type (`link`, `typed`, `auto_bookmark`, `form_submit`, and others). The worker keeps a map from each tab to its current document's provisional artifact in `chrome.storage.session`, built from the same `onCommitted` and `onHistoryStateUpdated` events it already handles. An entry is cleared when that tab moves to a page that is not an artifact, so a new tab opened from a Jira filter page is never credited to whatever ticket that tab showed before. Edges are captured when they happen:

- **New tab or window.** `webNavigation.onCreatedNavigationTarget` fires when a link opens a new tab or window (middle-click, `target=_blank`, `window.open`). It gives the new `tabId` and the `sourceTabId`, and the worker looks up the source tab's artifact at that moment. The worker stores it as the new tab's pending source, and the new tab's first recognized commit consumes it. Reading an opener tab's URL later would record the wrong edge whenever the opener had navigated since, which is common when the user middle-clicks several results from a list and keeps going. This event is part of `webNavigation`, which logging already requests, so `openerTabId` and its untested readability without `tabs` are not used.
- **Same tab.** The map's previous entry for that tab is the source of a link or form navigation.

A source is recorded only when it is a recognized artifact, which means it is on a granted origin; the worker never sees any other URL. The host re-resolves both ends at delivery, which gives edges such as "opened PROJ-123 from PR #412". An adapter's `refs` add *mentions* edges, for example an issue key in a pull request title. A ref scans only the extracted title, so an artifact never refers to itself through its own title prefix, matches issue keys only in uppercase, keeps at most 20 distinct refs per event, becomes a shown edge only once its target has been recorded as an artifact (so `UTF-8` never appears as a Jira issue), and resolves to a known artifact only when exactly one enabled adapter of the target type exists and its `id` template has exactly one placeholder besides `{host}` (adapter-templates.md §10.1). Otherwise it stays an unresolved key. *Co-focused* edges, between artifacts focused on in the same hour, are derived in the index and never written to the log.

**Aliases (M1.5b).** Artifact IDs change. A Jira issue moved to another project gets a new key (PROJ-123 becomes OPS-45), and the old key redirects. GitHub repositories are renamed and transferred. Confluence Data Center reaches one page through `/pages/viewpage.action?pageId=`, `/display/SPACE/Title`, and `/x/` short links. Without aliases, an artifact's history would silently split in two. Brauser keeps each artifact under one canonical ID plus a set of aliases:

- **Detection.** An alias is recorded only from evidence that the two IDs name the same thing:
  - **Server redirect.** The worker pairs `webNavigation.onBeforeNavigate`'s requested URL with the committed URL in the same tab and frame. If the commit's transition qualifiers include `server_redirect` and both URLs are recognized as different IDs of the same adapter type, that is an alias. Both URLs are on granted origins; a redirect through an ungranted origin is never seen.
  - **Declared identity signal.** An adapter can name a second source for the ID. One is the tab title, for example `[OPS-45]` in the title while the URL still says `PROJ-123`. The other is one named `<meta>` element's `content`, for example Confluence Data Center's page-ID meta tag on a `/display/` URL. This is the only case in which the fixed extractor (Tier 1) reads an attribute, and it reads only the `content` of `meta` elements the adapter names. Both signals come from text the page controls, so they are recorded as **suggested** aliases: they change record keys only after the user confirms them on the artifact card, or after a server redirect between the same two IDs corroborates them (adapter-templates.md §9.3).

  An SPA route change from one recognized ID to another is never treated as an alias, because it looks the same as clicking through issues in a board modal.
- **Recording.** The worker adds `{from, to, evidence}` to the hour's activity total, and the host writes it as an append-only `alias` event, after re-checking that both IDs are ones the confirmed adapter produces. A URL form that carries no stable ID, such as `/display/SPACE/Title` without the meta signal, is recorded under a provisional ID of that form and joins the canonical artifact once an alias names it.
- **Resolution.** The index groups confirmed and corroborated aliases transitively; suggested aliases are listed on the artifact card but not grouped. The ID most recently seen as a redirect target becomes canonical, so the new Jira key wins, and a later move back is handled the same way. Activity, edges, notes, and refs recorded under any alias count toward the canonical artifact, and a `refs` match on an old key resolves through the alias table.
- **Files.** The artifact file lives at the canonical ID's path. When an alias changes the canonical ID, the file at the old path is replaced, under the checked rename (§4.4), with a short Brauser-owned stub (`brauser.kind: artifact`, `alias_of: <canonical id>`) that links to the new file, so links in the user's own notes still lead somewhere. The frontmatter of the canonical file lists `aliases`.
- **Correction.** A wrong alias would merge two histories, so the artifact card shows "Also known as PROJ-123" with **Not the same**, which appends an `unalias` event. The log stays append-only, and the index honors the latest `alias` or `unalias` event for each pair.

**Mode.** Many tools reveal a mode in the URL: Google Docs `/edit` versus `/view` or `/preview`, Confluence's edit paths, and GitHub's `/files` review tab. An adapter maps these URLs to `view`, `edit`, or `review`. The record states what the URL showed: "opened in edit mode" is recorded, but "edited" is not claimed. Google Docs, for example, serves `/edit` to many view-only readers.

**Why I'm here.** The artifact card has an optional one-line note. It is appended to the log as its own event, and the latest one is shown. It is separate from the page note (§5.3). Most people will skip it, but when they do write one it is often the most useful entry in the log.

**Recent and worklog.** Every tool being adapted already has its own "recently viewed" list. What Brauser adds is one list across all of them, and that has to be visible in M1.5a, not only on the artifact card of a page the user is already on:

- **Recent tab.** The panel lists artifacts grouped by local day, newest day first, and within a day sorted by focused time. It can be filtered by type (for example, only `jira.issue` or `github.pr`) and by a title substring. Each row shows the title, type, focused and background minutes, and the one-liner if there is one, and opens the artifact's last URL. It reads the index (§5.5) through a `list_artifacts` message, so it needs no FTS5.
- **Worklog.** An optional daily or weekly summary, generated by the host from the index without an agent. It uses the same measured wording as the rest of the log, for example: "Opened 9 artifacts, and focused on 2 left open from an earlier day. PROJ-123 *Release blocker — SBOM export times out*: focused 2 h, opened from PR #412. Why: blocking the release." The panel shows yesterday's, today's, or this week's worklog with Copy, for standups and status reports. When the user turns on worklog files (M1.5b), the host also writes Brauser-owned `worklog/<YYYY-MM-DD>.md` or `worklog/<YYYY>-W<ww>.md` files (`brauser.kind: worklog`). They are regenerated lazily and skipped when unchanged, like artifact files (§6.7), and are not rewritten after their period ends unless `brauser reindex` runs. Worklog files are off by default, and the settings page shows the sync caution below before they are enabled.

**Sync caution.** Titles of confidential tickets and documents now sit in the notes folder. The setup confirmation for the first adapter says what artifact logging records (IDs, titles, Tier 1 fields, times, and trails) and, when the chosen folder is inside a known sync location (iCloud Drive, OneDrive, Dropbox, Google Drive), names that location. It warns and never blocks.

**Rules carried over from M1.**

- Logging is **opt-in**: the user enables an adapter for an exact origin and grants that origin from a setup action. The host independently checks scheme, host, and the adapter's match rules before recording a visit; Chrome's origin permission does not enforce URL paths.
- Incognito and private windows are never logged, regardless of config. The extension manifest uses `incognito: "not_allowed"`, and the event handler resolves the tab's incognito state and rejects it defensively.
- A URL no enabled adapter recognizes is dropped by the worker and never buffered. If one arrives anyway (a stale lease, an engine mismatch, or a compromised worker), the host returns the terminal outcome `suppressed` with reason `not_an_artifact` and writes nothing to the notes folder.
- **Buffer pressure.** The buffer holds only artifact events, so it fills far more slowly than a buffer of every visit on an origin would. Hourly activity totals (Coarse log, above) keep it small: each artifact adds at most one buffered event per hour, however often it is opened or switched to. At 80% of either the count or byte cap, the worker sets the toolbar badge (`action.setBadgeText`, no permission needed) to prompt the user to open the panel. Overflow is never silent: the dropped count and time range are kept and shown in the panel's capture strip, and an overflow marker is appended to the log on the next delivery.
- `/log` (§5.6) stays a one-off action on any HTTP(S) page. It records a `web.page` artifact keyed by the normalized URL.
- M1's site entries keep their grants and confirmations after the upgrade but record nothing until an adapter is enabled for the origin, and the settings page says so. Existing daily logs are left as they are, and the indexer reads their M1 visit entries as `web.page` visits.

**M1 mechanics, unchanged.**

- M1 observes main-frame `webNavigation.onCommitted` and `onHistoryStateUpdated` events, using document identity and lifecycle to avoid subframes, redirects, and duplicate SPA records. It does not keep dwell or scroll timers only in service-worker memory. A committed document's title arrives after commit, and an SPA usually sets a new route's title after `pushState`, so the worker briefly holds every new visit and fills its title from the tab's title update; the event is frozen once handed to the panel because the host hashes it for interrupted-write recovery. Each visit carries its local UTC offset, and the daily log uses that local calendar date.
- M1 normalization drops fragments and configured tracking parameters after host authorization. Artifact adapters (§7.2) do not rewrite URLs. They map a URL to an artifact ID, and the log keeps the normalized URL alongside that ID.
- Deliveries are idempotent by event ID recorded in the Markdown log. The host keeps a durable per-event intent in its OS config directory to find the original daily log across date and `log_dir` changes; only a complete Markdown marker proves persistence. Shared locks cover the current policy check, intent, append, and file sync, so revocation and concurrent panel connections cannot race a write. An interrupted append can resume only from an exact byte prefix in the still-selected folder; changed or missing logs require review. In M1, near-repeat visits within a configurable window (`near_repeat_secs`) are suppressed rather than rewriting a prior Markdown entry; the daily log remains append-only. M1.5a retires the window: hourly totals already give each artifact at most one line per hour, and a `/log` entry is a deliberate act that should never be dropped, while a replayed delivery is absorbed by its event ID. Dwell time is now kept as hourly activity totals (Engagement and Coarse log, above). With artifact logging, one intent record covers a whole delivery batch. The per-event intent journal is M1's, and compacting it moves from M5 into M1.5a (§12.2 step 6.5), because artifact logging is always on. Scroll depth is not planned.

### 5.2 Summaries

1. The extension extracts readable content (Readability-style) from a tab on demand. Any HTTP(S) page can be summarized. Without `tabs`, the side panel cannot see an ungranted tab's URL, and a side-panel click does not grant `activeTab` (§8). Summarizing therefore starts in one of two ways:
   - A trigger that grants `activeTab`: the `summarize` keyboard command, the page context menu's "Summarize with Brauser", or the toolbar action. The grant is temporary, needs no prompt, and covers only that tab until it navigates. The worker records the tab and document the grant was issued for, opens or focuses the panel synchronously in the handler (`sidePanel.open`, before any await), and the panel summarizes only that target.
   - The panel's Summarize button, when the extension already holds the page's exact origin or an `activeTab` grant for it. On any other page the button tells the user to use the shortcut, context menu, or toolbar action. It never asks for broad access, and it does not use the logging-only `webNavigation` permission to learn tab URLs.

   A trigger for summarizing does not enable logging; the host's site rules still decide what is logged.
2. The panel sends the tab's URL to the host, which answers `allowed` or `denied` from the denylist (§4.5). On `denied` nothing is extracted. Otherwise the extractor runs in the main frame only and returns `location.href`, `document.title`, and the content together. If the canonical form of the returned URL differs from the one checked, the panel discards the content and reports that the page changed. The host checks the returned URL against the denylist again, derives the summary's record key (§6.2) from it alone, and invokes the configured harness with the summary prompt.
3. Output streams back to the sidebar in chunks, staying under the 1 MB native messaging limit per message. Granularity depends on the harness (§7.1). Agent output is untrusted: the panel renders it only as text (`textContent`), with no Markdown-to-HTML conversion, so it loads nothing and never navigates by itself. Streamed text is provisional; a `rejected` run (§7.1) clears it.
4. The host holds no config or capture lock while the harness runs. It snapshots the config revision and harness entry at spawn. Before writing, it takes the config lock and a per-page summary lock, re-reads config, and rechecks the notes-folder identity, the summaries location, the denylist for both the original and the returned URL, and that the harness entry still matches. Any failure discards the output and nothing is written. The per-page lock serializes concurrent summaries of one page; the last to finish replaces the file. Canceled, timed-out, over-cap, rejected, non-zero-exit, and panel-closed (EOF) runs write nothing.
5. The host writes the final summary itself, to a summary file that is a separate record from the page note (§6.1). The note holds only the user's own text; Brauser never writes a summary into it. The host writes a summary only from a harness run it started itself for that URL; no message lets the extension supply summary text. A summary may be written for any HTTP(S) URL that passes normalization and the denylist, whether or not its site is enabled for logging, and it never creates a page note, log entry, or visit intent. Each page has one summary file, `<summaries_dir>/<slug>.summary.md`, keyed by the same record key as its page note (§6.2), so a document's `/edit` and `/view` URLs share one summary; the fixed suffix means no profile filename template can make it equal a note name. Its host-written frontmatter records `brauser.kind: summary`, the canonical URL, and the record key (§6.6), and its first line links to the page and to the page note's computed path in the profile's link style. The note link resolves once the note exists; changing `pages_dir` or `summaries_dir` leaves it stale until the page is summarized again. Summarizing again replaces the whole file (§4.4). Summary files belong to Brauser, and user edits to them are replaced. A file without a matching summary identity is a conflict and is never overwritten. The ownership check reads only the leading host-written frontmatter, so agent text cannot change a file's identity.
6. Before writing, the host neutralizes every construct that could make a Markdown viewer fetch a remote resource: inline images, reference-style images with their link definitions, and raw HTML tags such as `<img>`, `<iframe>`, `<video>`, `<audio>`, `<object>`, `<embed>`, `<link>`, and `<style>`. Each becomes an inert code span with its URL still visible. Tests cover each construct in Obsidian and VS Code preview.
7. The final message is `summary_result`, correlated by `request_id`, sent after the write is attempted. Its `outcome` is `created`, `replaced`, `created_with_warning`, `replaced_with_warning` (the file is in place but temp cleanup or directory sync failed, as with page notes; not a failure and not retried), `conflict` (names the occupying file and says to move or delete it), or `retryable` (a transient vault error before publication), with `relative_path` and `message`. After `conflict` or `retryable`, the host keeps the final text in memory, keyed by `request_id`, until the write succeeds, the panel discards it, or the connection closes; Retry sends only the `request_id` and does not rerun the harness. The panel keeps the text on screen with Copy, Retry save, and Discard; if the connection is lost it becomes a copy-only unsaved view. A canceled, timed-out, or failed run shows its partial text marked unsaved.

### 5.3 Page notes

- A page note is a persistent scratch space attached to the page. The user can add to it at any time; when they return to the page, the panel shows it again.
- One markdown file per record key: per artifact on a recognized page, otherwise per normalized URL (§6.2). A Google Doc has one note whether it is open in `/edit` or `/view`. It is created on the first keystroke, not when a page is opened, so visiting a page never creates an empty note.
- Notes work on any HTTP(S) page, not only logged sites. The panel cannot see an ungranted tab's URL (§5.2), so the first note on a site starts, like summarizing, from the toolbar action, a keyboard command, or the page context menu, which grant `activeTab`; the panel then requests that exact origin from Chrome so it can read the tab's URL and title on later visits. A grant made for a note does not enable logging, and removing a logging site keeps it; the extension records the origins the panel requested so the settings page can tell them apart.
- The note's title comes from the page's title when the note is created (or its host name when the page has none); the panel asks only for the note text.
- The panel autosaves: it sends the whole note about one second after the user stops typing, and again when the active tab changes or the panel closes. There is no Save button. A small indicator shows "Saving…", "Saved", or an error.
- The side panel is the only supported editor. The files stay plain Markdown and can be read anywhere, but an edit made outside Brauser can be overwritten. The version check in §4.4 exists so two Brauser windows do not overwrite each other: when a save is refused, the panel loads the newer note and shows the text it could not save so the user can copy it back.
- A page note holds only the user's thoughts. Agent output never goes into a note; it goes to the page's separate summary file (§5.2) or wherever a command sends it (§5.6).
- Notes created by M1 load into the editor normally. M1 review drafts are left in place and are not merged.

### 5.4 Read later

- `/save` writes a read-later entry, one per record key (§6.2); saving an already-saved artifact from another of its URLs reports it as already saved. It may link to the page's summary file (§5.2); the summary itself is never copied into the entry.
- Items can be marked done, which moves them in the index but never deletes the file.

### 5.5 Related pages

- The host maintains a SQLite FTS5 index over titles, summaries, notes, and tags.
- When the active tab changes, the sidebar shows related pages ranked by text relevance, shared tags, and shared domain.
- v1 is keyword search. Embedding-based similarity is a later, optional stage.
- The index also holds `artifacts`, `visits`, and `edges` tables, built from the artifact log (§5.1) and arriving with M1.5a before FTS5. This lets the omnibar (§5.6) answer questions that browser history cannot, such as "the Confluence page about SBOM I read last week", "PRs I reviewed this sprint", and "everything connected to PROJ-123".
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
| `/log` | Log the current page now as a `web.page` artifact, on any HTTP(S) page, even if no adapter recognizes it (§5.1) |
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

`/log` and notes are one-off actions the user takes on a single page, so they need Chrome's grant for that site but not the host's native confirmation (§7). `/log` records the page as a `web.page` artifact: one `activity` event in the log, and an artifact file and index entry like any other artifact (§5.1). It does not enable automatic logging for the site.

### 5.7 Side panel layout

The panel is about the page in the active tab. Capture housekeeping collapses into a one-line strip at the bottom, and configuration stays on the settings page.

```
┌────────────────────────────────────────────┐
│ Rauser Browser Browsing Assistant       ⚙  │  gear opens settings
├────────────────────────────────────────────┤
│ Designing Data-Intensive Apps — Ch. 5      │  current page; follows the
│ oreilly.com/library/view/…/ch05            │  active tab
│ ● Artifact logged  ✦ AI allowed   ☆ Save   │  status chips; ☆ = /save
├────────────────────────────────────────────┤
│ ARTIFACT  jira.issue PROJ-123   In review  │  only on a recognized page
│ Focused 2 h over 5 days · last Tue         │  (§5.1); Tier 1 fields
│ ← opened from PR #412 · 3 linked           │  when configured
│ Why: blocking the release             ✎    │  optional one-liner
├────────────────────────────────────────────┤
│ [ This page ]   Recent   Read later (3)    │
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

The Recent tab (M1.5a, §5.1) lists artifacts by day, with a type filter, a title filter, and a Worklog view with Copy. The Read later tab lists saved pages with a search box and a checkbox to mark each done; done items move to a collapsed "Done" group and their files are kept.

Each state changes one area rather than the layout:

| State | Panel |
|---|---|
| Not set up | The setup warning replaces the tabs; the omnibar and page controls are disabled; the strip reads "Off". |
| Native host unavailable | A banner under the header with the fix; everything below is disabled. |
| Origin not enabled | Chip `○ Not logged · Set up artifacts for this site…`, which opens settings with the origin filled in and offers any matching starter adapter. Notes, `/log`, and summaries still work. |
| Enabled origin, page not an artifact | No artifact card; chip `○ Not an artifact`. When unrecognized pages recur, a hint offers to draft an adapter (Tier 2, §5.1). |
| Site on the agent denylist | Chip `⊘ AI off for this site`; summarize and questions are disabled with the reason. |
| No agent set up | Chip `✦ AI not set up`; the omnibar offers only commands that need no agent, plus a link to settings. |
| Not an HTTP(S) page | The page area names the tab and says Brauser can't use it; only Read later is active. |

Milestones fill the layout in: the page area, My note, and the capture strip first; the artifact card, the Recent tab, and the worklog view in M1.5a, and worklog files in M1.5b; the answer card, Summary, and omnibar in M2; Related and Read later in M3; the full command list in M4.

## 6. Data Format

### 6.1 Folder layout

The user chooses the notes folder at setup. It can be empty, or a subfolder of an existing knowledge base. The four content locations are configurable; the layout below is the suggested starting point offered at setup, not a requirement.

```
<notes folder>/                  chosen by the user, no default
  log/2026/09/2026-09-27.md      daily artifact log: append-only source of truth
  artifacts/<type>/<site>/<slug>.md  one Brauser-owned file per artifact, regenerated from the log (M1.5a)
  worklog/2026-09-28.md          optional Brauser-owned daily or weekly worklog (M1.5b, off by default)
  pages/<slug>.md                page note: the user's text only
  summaries/<slug>.summary.md    one Brauser-owned summary per page (M2)
  later/<slug>.md                read-later items
```

`artifacts_dir` and the optional `worklog_dir` are further content locations under the same no-overlap rule. Artifact paths are built by the host from slugs of the adapter type, the site host, and the artifact ID; the captured ID is never used as a path directly. When slugging an ID would lose information, for example a case-sensitive Google Docs ID on a case-insensitive filesystem, the host appends a short hash of the full `artifact_id` so two artifacts never share a file.

No content location may equal, contain, or sit inside another, compared case-insensitively; the host rejects an overlapping config. An M1 config has no `summaries_dir`: capture keeps working, `/summarize` reports `not_configured`, and the first harness setup proposes `summaries` in its native confirmation (§7.3). An overlap found then is a recoverable setup error, never resolved silently.

Brauser's own files live outside the notes folder, in standard OS locations, so it never adds hidden folders to someone's knowledge base:

| File | Location |
|---|---|
| `config.toml` | OS config directory (e.g. `~/Library/Application Support/Brauser` on macOS, `%APPDATA%\Brauser` on Windows) |
| `visit-ids/` | OS config directory; M1's one synced intent file per persisted visit, replaced in M1.5a by one intent record per delivery batch and a compact index (§12.2 step 6.5) |
| `index.db` | OS cache directory |

If Brauser finds an existing folder with files it didn't create, it leaves them alone. It only indexes files under its configured content locations.

### 6.2 Internal model

The host works on two record types. A `PageNote` has `url`, `canonical_url`, a record key (`artifact_id` or `url_id`, below), `title`, `created`, `updated`, `tags`, `links`, and `body`. A `Summary` has `url`, `canonical_url`, a record key, `title`, `generated`, `harness`, `harness_version`, `note_link`, and `body`; it is Brauser-owned and replaced whole (§5.2). An `Artifact` has `artifact_id`, `type`, `site`, `title`, `url` (the last canonical URL seen), `fields` (Tier 1), `first_seen`, `last_seen`, `focused_secs`, `background_secs`, `opens` (cold opens, §5.1), `days_opened` (days with at least one open), `last_mode`, `why`, `aliases`, and `edges`. It is derived from log events (`activity`, `why`, `alias`, `unalias`) and regenerated, never edited in place. The index joins notes, summaries, read-later entries, and artifacts by record key.

`artifact_id` has the form `<system>:<host>/<native id>`, for example `jira:acme.atlassian.net/PROJ-123`, `confluence:acme.atlassian.net/123456`, `gdoc:docs.google.com/<id>`, or `github.pr:github.com/owner/repo/412`. The native ID is the tool's own identifier, with case normalized only where the tool treats it as case-insensitive (Jira keys are uppercased). Other systems that already know these artifacts can then join on the ID without Brauser fetching anything. The scheme is fixed by each adapter's `id` template and is part of the adapter's documented contract. Profiles only control serialization, including how `note_link` is rendered.

Because `artifact_id` is the record key below, an `id` template is durable. The adapter template `format` number versions the language, and a later format never changes how a template in an older format builds an ID. Changing the `id` template of an adapter that has recorded activity is a **migration**: the host shows how many artifacts are affected and, on confirmation, writes one `alias` event from each old ID to its new one, so no history, note, or summary is orphaned (adapter-templates.md §8.2). Aliases arrive in M1.5b, so until then the host refuses an `id` change on an adapter with recorded activity.

**Record keys.** Every per-page record (page note, summary file, and read-later entry) is keyed by `artifact_id` on a recognized page and by `url_id` otherwise. Without this, one document gets several notes and several summaries: Google Docs `/edit` and `/view` normalize to different URLs, and a Jira issue opened in a board modal and at `/browse/` does too. The rules:

- **The host derives the key; nothing else supplies it.** The extension sends only the URL. The host runs that URL through the confirmed Tier 0 adapter rules in its config, resolves aliases (§5.1) to the canonical ID, and uses the result. Tier 1 fields and page content never affect a key, so a page cannot choose which note or summary it gets. The adapter rules apply for keying whether or not the worker is logging that origin right now, so turning logging off does not split a note.
- **Both keys are recorded.** A record keyed by artifact records `artifact_id` and the `canonical_url` it was created on. A URL-keyed record records `canonical_url` and `url_id`, as in M1. Filenames carry a stable suffix derived from the key, so a title slug never decides identity. The ownership check matches the recorded key (§6.4).
- **Aliases follow the artifact.** A record stays at the filename it was created under. When the index resolves its `artifact_id` to a newer canonical ID, lookups for either ID find it. If both IDs already had a note before the alias was learned, the panel shows the canonical note and lists the other as "note from before the move"; nothing is merged automatically. An `unalias` separates them again. When a note is reached through an alias, the panel says so.
- **Older URL-keyed records stay findable.** Notes from M1, and notes made before an adapter existed, are left where they are. At reindex, the host maps each URL-keyed record's `canonical_url` through the current adapters, so the panel lists every older note on any URL of the artifact as "older note on this URL". It is still editable in place. When the artifact has no note yet, **Use as artifact note** rekeys one: the host writes it under the artifact key with a no-clobber create, then removes the URL-keyed file after checking its identity. If an adapter is later removed, an artifact-keyed record is still listed on the URLs it recorded.
- **Keys are checked at write.** `load_note` returns the key it resolved, and `save_note` sends it back. If the host's current config maps the URL to a different key (an adapter or alias changed in between), the host refuses the save as `note_conflict` with reason `key_changed`, and the panel reloads and keeps the unsaved text in the copy-out box. A summary's key is recomputed under the config lock before writing (§5.2 step 4); a changed key discards the output, like any other config change during a run.

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
artifacts_dir = "artifacts"          # M1.5a; proposed when the first adapter is enabled
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

Page-note and summary filenames include a stable identifier derived from the record key (§6.2), even when a profile uses a title slug. Before updating a file, the host verifies that the recorded key (`artifact_id`, or `canonical_url` and `url_id`) and Brauser ownership match the requested page. An absent or different identity is a conflict, never an opportunity to adopt or overwrite an unrelated file. Records carry `brauser.kind`: `summary` for summary files; a page note without a `kind` key, as all M1 notes are, is a `page`. The page-note path rejects a file whose kind is `summary` or `artifact`. The summary path accepts only a file with exactly one `kind: summary` line, and the artifact path accepts only a file with exactly one `kind: artifact` line.

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

A note on a recognized page (M1.5) records `artifact_id` as its key instead of `url_id`; `canonical_url` is the URL it was created on:

```yaml
brauser:
  artifact_id: gdoc:docs.google.com/1AbC
  canonical_url: https://docs.google.com/document/d/1AbC/edit
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

### 6.7 Example artifact file (neutral profile, M1.5)

```markdown
---
title: Release blocker — SBOM export times out
url: https://acme.atlassian.net/browse/PROJ-123
brauser:
  kind: artifact
  artifact_id: jira:acme.atlassian.net/PROJ-123
  type: jira.issue
  status: In review
  first_seen: 2026-09-22
  last_seen: 2026-09-29
  focused_min: 121
  background_min: 35
  opens: 14
  days_opened: 5
  last_mode: view
  aliases: [jira:acme.atlassian.net/OPS-45]
---

Why: blocking the release

## Timeline

- 2026-09-29 · focused 42 min · 15 min in background · opened 3× · from [PR #412](../../github.pr/github-com/owner-repo-412.md)
- 2026-09-25 · focused 12 min
- 2026-09-24 · focused 1 min · opened 1× · typed
- Week of 2026-09-14 · focused 38 min · opened 6× on 2 days

## Linked

- mentions ← [PR #412 Fix SBOM export pagination](../../github.pr/github-com/owner-repo-412.md)
- opened → [SBOM export design](../../gdoc/docs-google-com/sbom-export-design-3f9a1c.md)
```

Artifact files belong to Brauser and are rebuildable in the same way as the index: `brauser reindex` regenerates them from the log, and user edits to them are replaced (§4.4).

Artifact files are written to be quiet in synced and versioned folders:

- **Lazy regeneration.** Delivery updates the index and marks changed artifacts dirty there; it does not touch their files. The host regenerates dirty files on a best-effort pass when the panel's connection closes, on the first connection of each local day, and when the user chooses "Update artifact files now" in settings. The dirty set lives in the index, so a host that exits before its pass loses nothing. The panel's artifact card reads the index, so a file that is not yet regenerated never makes the panel stale.
- **Stable content.** Frontmatter uses dates, not timestamps, and whole minutes. A file whose regenerated bytes are unchanged is not rewritten (§4.4). An extra visit in the same day changes one Timeline line and a few counters, not the whole file.
- **Bounded size.** The Timeline keeps one line per day for the last 14 days with activity. Older days roll into one line per week, and weeks older than 26 weeks roll into one line per month. Linked lists the 50 strongest edges and ends with "N more" pointing at the omnibar. The full detail stays in the log and the index, so a ticket opened every day for a year still has a file of a few dozen lines. Titles, Tier 1 fields, and the one-liner are written with YAML escaping, and their Markdown is neutralized as in §5.2 step 6, so a hostile ticket title cannot make a viewer fetch a remote resource. Links follow the profile's link style.

## 7. Configuration

Config lives in the OS config directory (§6.1) and is owned by the host. The extension reads and edits it only through host messages, which validate every change and require the revision returned by `get_config`. In M1, a change to `storage.root` requires a short-lived, single-use token from a native folder picker. `update_config` must reject an arbitrary root path supplied by the extension. Adding or widening a site matcher, enabling capture, or weakening a privacy exclusion also requires a native host confirmation and a short-lived, single-use token bound to the exact config delta and current revision. Narrowing or disabling capture can take effect immediately. One-off actions the user takes on a single page, such as writing a note or running `/log`, are not config changes and need no native confirmation: the click or keystroke is the consent, and Chrome's site grant still applies. A Chrome grant is a separate browser access gate and does not authorize a host config change. Canceling a picker or confirmation leaves config unchanged. A malformed or newer on-disk config must produce a recoverable setup error rather than preventing the host from responding at all.

The config starts nearly empty. Setup writes only what the user chooses; everything else is either off or an inert, commented-out example. The host refuses to perform an action whose required config is missing and tells the extension what to configure, rather than guessing. Configuration is edited on the extension's settings page; the side panel links to it.

### 7.1 Agent harnesses

An agent is optional. Without one, logging, notes, read later, and related pages all still work; only AI commands are disabled.

Brauser ships harness templates for Claude Code, Codex, and a generic CLI, but configures none of them automatically. At setup, the host may look for known harnesses on `PATH` and offer what it finds; the user confirms the binary path and chooses one. Nothing is enabled without that confirmation.

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

The argument lists above follow the Claude Code 2.1.283 and codex-cli 0.144.4 documentation and help output (checked 2026-09-28). Trial runs of both harnesses the same day informed the rules below; the templates have not yet passed M2 acceptance. Each harness run starts in a new empty host-owned working directory of its own (under `<config dir>/agent-work`, mode 0700, removed after the run, so concurrent host processes never share one), so no project instructions or project config load and Claude Code does not leave a new per-directory entry under `~/.claude/projects` on every run. `--max-turns` is omitted: with no tools there is no second turn, and it is not listed in `--help`.

**Authentication and environment.** A Chrome-spawned host does not inherit the login shell's environment, and `--setting-sources ""` skips the `env` block in `~/.claude/settings.json`, so Bedrock, Vertex, or API-key users need their variables allowlisted. Setup finds out how the harness authenticates and proposes the needed names (for example `CLAUDE_CODE_USE_BEDROCK`, `AWS_PROFILE`, `AWS_REGION`, `ANTHROPIC_API_KEY`); the native harness confirmation shows names, never values. Setup then runs a one-token probe with the full argv and refuses the harness if it fails.

**Isolation.** `HOME` is passed only so the harness can find its credentials, but it also exposes user-level instructions, settings, hooks, MCP servers, plugins, and memory, so isolation must be proven, not assumed. Claude Code with `--tools ""` and `--disallowedTools "*"` has no tools; if `--setting-sources ""` is rejected, `--safe-mode` also stops CLAUDE.md, hooks, MCP servers, and plugins from loading. Administrator-managed settings still apply. The host parses Claude Code's `system`/`init` event and kills the process tree if `tools`, `mcp_servers`, or `plugins` is non-empty. Claude Code reads stdin to EOF before emitting it, so this limits what a misconfigured run can do and return; it does not keep page content from the harness. Codex still loads the user's global `AGENTS.md` and user skills even with `--ignore-user-config`. The host runs Codex with the user's own `HOME` and `CODEX_HOME`, so it shares the user's normal Codex login and never copies or refreshes a credential of its own; the accepted cost is that user-level Codex instructions and skills can shape Brauser's output (§14). The Codex harness confirmation and the settings page say so, and the Claude Code harness remains the recommended one. M2 acceptance includes a canary run for each template against a scratch `HOME` containing real credentials plus a user instruction file saying "reply CANARY", a hook that writes a marker file, a user MCP server, a user skill, and a memory entry. The Claude Code template passes only if none has any effect. The Codex template passes only if the hook, the MCP server, and the memory entry have no effect and any tool use a skill attempts ends the run as `rejected`; the instruction file and the skill's text are expected to reach Codex, and the canary records that they do, so the documentation stays accurate.

**Tool use.** Codex documents no switch that removes every tool, and its feature names change between versions. The host accepts a Codex run only while every event is on an allowlist recorded for the harness version: thread and turn lifecycle, usage, errors, and `item.*` events whose `item.type` is `agent_message` or `reasoning`. At the first other event, known or unknown, the host kills the process tree, sends the panel a `rejected` message that clears any streamed text, and writes nothing. A chunk is forwarded only after its event passes the allowlist. For Claude Code, any `tool_use` block does the same. This is detection, not prevention: a Codex tool can read local files before its event arrives. The read-only sandbox is what prevents writes, and disabling `shell_tool` and `unified_exec` is what prevents commands. Setup therefore also passes `--disable` for every feature `codex features list` reports enabled and not needed for text output (at 0.144.4 this includes `in_app_browser`, `browser_use_external`, `browser_use_full_cdp_access`, `code_mode_host`, `goals`, `remote_plugin`, `plugin_sharing`, and `tool_suggest`), and refuses a Codex version that reports an enabled feature not on the reviewed list. Claude Code, which runs with no tools, is the recommended harness for page content.

**Checks and results.** At setup, and again before any run where the binary's identity (resolved real path, size, and modification time) has changed, the host runs `--version`, confirms in `--help` (and `codex features list`) every flag it will pass, and records the results. Both harnesses update themselves in place, so this re-check matters. A passing re-check continues without asking the user again; a failure, or a real path outside the confirmed install location, disables AI commands and the settings page shows why. The host never falls back to other flags. A run succeeds only when the process exits 0 and, for Claude Code, the final `result` event has `is_error: false` (an auth failure can report subtype `success` with `is_error: true`); error text is never shown as summary content. stderr is kept for diagnostics only, since Codex writes ERROR lines on successful runs. Streaming granularity differs: Claude Code's `stream-json` emits token deltas, while `codex exec --json` 0.144.4 emits only whole items, so the panel shows progress until the `agent_message` arrives. Cancellation kills the process tree either way.

**As built (M2 step 1, protocol v4).** These points refine the plan above; the rest of it is still the target.

- `config.toml` holds one harness, not a table of them: `[agent]` names the `default` and `[agent.harnesses.<id>]` holds only that entry. A setup replaces it; removing it (from settings) leaves `agent_denylist_confirmed` set. Beside it the host writes a host-only `[harness_record]` table: the resolved real path, size, modification time, file ID, version, a hash of the `--help` output, the flags it confirmed there, and when the probe passed. The record never goes over the wire; the extension sees `agent_status` instead. A bad harness entry or record disables only AI commands; capture keeps working.
- Discovery searches the absolute `PATH` entries the host was started with plus `$HOME/.local/bin`, and nothing else. A binary and its directory must be owned by the user or root and not group- or world-writable. `--version` and `--help` run with only `HOME` and `PATH`. Offers report each optional environment name (for Claude Code: `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `AWS_PROFILE`, `AWS_REGION`, `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`) as present or not, never its value.
- The probe runs after the native confirmation, not before it, because it sends a prompt (a fixed test string, never page content) with the confirmed environment. A failed probe mints no token and changes nothing. The probe passes only on exit 0 with every stdout line a JSON object and a final `result` event with `is_error: false`. The harness confirmation is the same native Yes/No dialog as the settings confirmation, so it also closes when the host goes away or after 270 seconds; a confirmation that closes that way spends the offer and runs nothing, and the settings page waits six minutes for the dialog plus the probe's 60-second cap. A host killed mid-run leaves its work directory behind; host startup removes generated ones older than an hour, like its other temporaries.
- Claude Code 2.1.284's `--help` lists `--setting-sources <sources>` but does not document the empty value that `--setting-sources ""` relies on. Only the probe shows it is accepted; the `--safe-mode` fallback above is not built, so a harness that rejects it is refused.
- Codex setup runs `codex --version` (exactly `codex-cli N.N.N`) and refuses a version with no reviewed feature tables. It then checks every flag in `codex exec --help` and runs `codex features list` with the same `--disable` pairs a run passes; a disabled feature still reported enabled, or an enabled one not on the reviewed list, is refused. `--version`, `exec --help`, and the probe get only `HOME`, `PATH`, and `CODEX_HOME` when the host's environment has it, the user's own, so Codex uses its normal login and Codex home (§14); setup then lists `CODEX_HOME` among the required names in the confirmation and records it in `env_allow`. `codex features list` has no `--ignore-user-config`, so it runs with `CODEX_HOME` set to an empty host-owned directory instead: the user's `[features]` in `config.toml`, which runs ignore, cannot change what the check sees. The Codex setup confirmation and the settings page say that user-level Codex instructions (`AGENTS.md`) and skills can shape its output, and that Claude Code is the recommended harness. The Codex probe passes only on exit 0 with every line a known `--json` event, only `agent_message` and `reasoning` items (any other item counts as tool use), at least one completed `agent_message`, and a final `turn.completed`; `turn.failed` or `error` is a reported error. The codex-cli 0.144.4 feature tables were read off one install and are still *pending review*.
- A real path outside the confirmed install location (a prefix check) is not built; the identity re-check covers in-place updates. The prefix check is deferred to M2 host invocation (§12.2 step 8.2).
- The settings page drives setup: it lists the offers, lets the user choose optional environment names, edits the denylist before setup (the list is saved only by the confirmed setup) and after it (adds save at once; removals get the native confirmation), and removes the harness. Edits stay live once setup has confirmed the list, even after the harness is removed.

Prompt injection in page content can still distort a summary or try to smuggle page data into URLs. With Claude Code it cannot run commands or write files. With Codex it cannot write files or run shell commands, and any other tool use aborts the run, though a tool may already have read a file. Neither the panel nor a stored summary loads remote resources (§3.1, §5.2).

### 7.2 Artifact adapters

An adapter recognizes artifacts. It does not clean up URLs for a visit log. Its job is to say "this URL is Jira issue `PROJ-123` in `acme.atlassian.net`," not "this is a page on an allowed site" (§5.1).

Adapters are written in **adapter templates**, a small declarative language specified in [adapter-templates.md](adapter-templates.md) (format version 1). Path templates match a URL's path one segment at a time, query conditions match named parameters, title templates extract the title and fields from the tab title, and `id` templates build the canonical `artifact_id`. Captures use a fixed set of host-owned types (`int`, `hex`, `token`, `name`, `jira_key`, `uuid`, and literal alternations). Raw regular expressions are not supported.

```toml
[adapters]
format = 1

[[artifacts]]
type   = "jira.issue"
origin = "https://acme.atlassian.net"
match  = [
  { path = "/browse/{key:jira_key}" },
  { path = "/jira/{*}", query = { selectedIssue = "{key:jira_key}" } },
]
id     = "jira:{host}/{key}"
title  = "[{key:jira_key}] {title} - Jira"
refs   = [{ type = "jira.issue", find = "jira_key" }]   # other artifacts this one's title mentions
fields = { status = "<status selector>" }   # Tier 1 (M1.5b), optional; real selectors are recorded per starter adapter

[[artifacts]]
type     = "gdoc"
origin   = "https://docs.google.com"
unscoped = "acknowledged"   # shared origin; see Scope below
match    = [{ path = "/document/d/{id:token(20,64)}/{mode?:(edit|view|preview)}" }]
modes    = { edit = "edit", view = "view", preview = "view" }
id       = "gdoc:{host}/{id}"
title    = "{title} - Google Docs"
```

- **Evaluation.** Every operation runs in time linear in its input, with no backtracking, by construction (adapter-templates.md §4.3, §9, §10.2). The host (Rust) and the worker (TypeScript) each implement the same small grammar directly; neither compiles templates to regular expressions. Both prepare the URL identically (WHATWG parsing, 2048-character cap, per-segment percent-decoding, and refusal of `%2F`, `.`, `..`, and control characters inside a segment) and cap titles at 512 scalar values. Matching runs against the original URL after host authorization, so a path template is also the adapter's path rule. The first matching rule of an adapter wins, and adapters on one origin are tried in config order. Config validation reports the first error with its position and caps adapters at 64 and rules at 16 per adapter.
- **The worker's copy.** The same confirmed templates go to the worker in the policy lease (§3.2). A disagreement between the engines can only cost a visit the prefilter wrongly dropped, never add one, because the host re-checks. Shared test vectors, differential property tests between the two implementations, fuzzing, and bounds tests run in CI (adapter-templates.md §12).
- **Captures.** Named captures fill the `id` template. `{host}` is the authorized origin's host. No capture type admits `/`, `:`, `%`, whitespace, or control characters, so a capture never becomes a path (§6.1) and an `artifact_id` splits back into its parts unambiguously. The title template runs on the tab title, and when it does not match, the whole tab title is used.
- **Scope.** Some origins host many unrelated tenants: every GitHub organization, every Google account in a Chrome profile, every Notion workspace. The host owns a list of **shared origins** (`github.com`, `docs.google.com`, `www.notion.so`, `www.figma.com`, and `linear.app` in format version 1). An adapter on a shared origin must either restrict captures to listed values with `scope` (for example `scope = { owner = ["acme-corp"] }`), or declare `unscoped = "acknowledged"` where the URL cannot tell tenants apart, as with Google Docs. The native confirmation for an unscoped adapter says plainly that it records every matching artifact from every account in the Chrome profile, and recommends a separate Chrome profile for work. A URL outside the scope is not an artifact.
- **`path_case`.** Literal path segments compare case-sensitively by default; `path_case = "insensitive"` folds ASCII case for tools such as SharePoint. Capture types define their own case handling.
- **Aliases.** An adapter may declare `alias = { from = "tab_title", capture = "key" }` or `alias = { from = "meta", name = "<meta name>", value = "{id:int}" }`, its second source of identity (§5.1). The value must produce an ID through the same `id` template. Server-redirect aliases need no declaration.
- **Tier 1 `fields`** (M1.5b) are optional named CSS selectors, used as in §5.1, and travel to the worker in the lease with the match rules. The fixed extractor caps each value (for example at 256 characters) and the total. When a selector matches nothing, the field is empty and nothing fails.
- **Overlap.** Because the language has no free-form patterns, the host compares rules segment by segment and warns when two adapters on one origin could match the same URL, naming the rule that wins. The same check rejects a Tier 2 draft that overlaps an existing adapter and feeds the match preview's re-map count.
- **Starter adapters.** Brauser ships adapters for common tools (Jira, Confluence, Google Docs/Sheets/Slides, GitHub pull requests and issues, Notion, Figma, Linear, SharePoint) as disabled **starter adapters** with a placeholder origin such as `https://<your-site>.atlassian.net`. Before enabling one, the user resolves it to an exact scheme, host, and port and can edit it for self-hosted tools. Tool UIs change, so each starter adapter records the date its URL and title formats were last checked against the live tool. A starter adapter's `id` template is frozen once it ships (§6.2). A Tier 1 selector that stops matching degrades to Tier 0; it never fails the visit.
- **Match preview.** Enabling or widening any adapter, not only a Tier 2 draft, shows the match preview from §5.1 against the origin's URL-shape sample and probe URLs. For a user-written adapter, the Tier 2 rejection rules only warn, because user-written config is trusted (§4.2).
- **Consent.** Enabling an adapter requests the origin's optional Chrome grant, when it is not already held, and the host's native confirmation. The confirmation shows the adapter's match rules, scope (or the unscoped statement), Tier 1 fields, and the §5.1 sync caution. Adding or widening an adapter's match rules, fields, or scope (adding a scope value, or switching to `unscoped`) widens capture and needs the same confirmation. Removing or narrowing them takes effect immediately. Changing the `id` template of an adapter that has recorded activity is a migration, not an edit (§6.2).
- **M1 compatibility.** M1's `[[sites]]` entries (an exact origin and an optional path prefix) remain valid config that grants an origin but records nothing on their own (§5.1).

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

Each entry is a hostname. The host stores it lowercased, converted to punycode, and without a trailing dot, and rejects entries with a scheme, path, port, or wildcard. As built (protocol v4), the host accepts only entries already in that form and rejects the rest; the settings page normalizes what the user types (lowercase, punycode, trimmed) and shows the result before adding it. Its suggested categories only fill the input box. In `config.toml` the privacy keys sit flat at the top level beside `strip_params`, matching the wire `ConfigSnapshot`, rather than in the `[privacy]` table shown above. An entry matches that host and every subdomain on any scheme and port; IP literals match exactly, by address: an IPv4 entry also matches the same address written as IPv4-mapped IPv6 (`[::ffff:a00:1]` for `10.0.0.1`), and the reverse. IPv6 entries use the URL (WHATWG) serialization the settings page produces, which writes an IPv4-mapped address in hex. Checks use the original URL before normalization.

## 8. Extension Permissions

| Permission | Why |
|---|---|
| `sidePanel` | Always-on sidebar |
| `nativeMessaging` | Talk to the host |
| `storage` | UI preferences and the capped visit buffer (§3.2) |
| `webNavigation` | Optional; requested only when the user enables the first artifact adapter. Provides main-frame commits, SPA route changes, transition types, and `onCreatedNavigationTarget` for trails (§5.1) |
| `scripting` | Required from M1.5b, which adds it to run the fixed Tier 1 field extractor from the worker on recognized artifact pages (§5.1); M2 reuses it to run the pinned content extractor in a tab under an `activeTab` grant or an enabled origin's grant. Adds no install warning without host permissions |
| `activeTab` | Required, brought forward from M2 by the side-panel rework (§12.2 step 4); grants temporary access to one tab from the `note` command, the page context menu, or the toolbar action. No install warning |
| `contextMenus` | Required, brought forward from M2 by the side-panel rework; the page context menu's "Add a note in Brauser" (M2 adds "Summarize with Brauser" alongside it) |
| `idle` | M1.5; stops counting attention when the user has been idle for the detection interval (5 min by default) or the screen locks (§5.1). No install warning |
| `commands` | A `note` entry, brought forward from M2, starts the first note on an ungranted page (§5.3); `summarize` remains M2 |
| `optional_host_permissions` | Manifest declares HTTP(S) patterns for sites discovered during setup; Chrome grants only the exact origin requested for an enabled logging site or a first note. Adapter paths remain host-enforced |

The manifest's broad optional HTTP(S) patterns permit runtime requests for user-chosen sites; no origin is granted at install time. Logging permission requests run directly from a settings-page button while Chrome still has the user gesture, before awaiting native confirmation. If native confirmation is canceled, the extension removes any newly granted origin and optional API permission. Chrome remembers granted optional permissions after the extension removes them, so a later request for the same origin can succeed without a new Chrome prompt (§12.1); the host's native confirmation, not Chrome's prompt, is the consent record for logging. Removing a grant stops capture for that origin and purges its buffered events. The service worker reads the title from tab metadata on granted sites; M1 requested neither `scripting` nor `activeTab`. The side-panel rework (§12.2 step 4) added `activeTab`, `contextMenus`, and a `note` command ahead of M2, and with them raised `minimum_chrome_version` from 114 to 116 (`sidePanel.open` from a command or context-menu handler needs Chrome 116). `scripting` arrives with Tier 1 in M1.5b (§12.2 step 7.3).

M2 adds a `summarize` entry under `commands`, reusing `scripting` from M1.5b and the `activeTab` and `contextMenus` permissions the side-panel rework already added. A side-panel click grants neither `activeTab` nor the tab's URL, so summarizing an ungranted page starts from the `summarize` command, the context menu, or the toolbar action (§5.2, §5.6), the same pattern notes and `/log` already use for the first note or log entry on a new site (§5.3). Notes and `/log` request the page's exact origin; summarizing does not. M2 does not add `tabs`, which Chrome labels "Read your browsing history", and makes no persistent origin grants for summarizing, so none accumulate and logging-site removal never affects summarizing.

M1.5a adds `idle` and uses `tabs.onActivated`, `tabs.onRemoved`, and `windows.onFocusChanged`, none of which needs `tabs`. Trails use `webNavigation.onCreatedNavigationTarget`, under the `webNavigation` permission logging already requests, and not `openerTabId` (§5.1). Tier 1 fields need `scripting` on an already-granted origin, called from the worker while the page is live. M1.5a also sets the toolbar badge (`action.setBadgeText`, no permission) when the buffer nears its cap. M1.5b adds `scripting` with Tier 1, ahead of M2 content extraction (§12.2 step 8.4), which reuses it; it adds no install warning without host permissions.

No broad host permissions are requested at install time.

## 9. Repository Layout

```
/extension        TypeScript MV3 extension
/host             Rust native host (workspace: protocol, vault, index, agent, cli)
/adapter-templates Rust crate for the adapter-template language (M1.5a; adapter-templates.md)
/protocol         Shared message schema (JSON Schema → generated TS and Rust types)
/installers       macOS and Windows user installers; optional Homebrew formula
/fixtures         Sample vault and captured pages for tests
/docs             DESIGN.md, adapter-templates.md, SECURITY.md, CONTRIBUTING.md, config reference
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
| **M1 — Capture** | Side panel and settings page, site logging, URL normalization, daily log, page notes (create-only with review drafts as first built; the side-panel rework before M2 made them editable and autosaving, §12.2 step 4) |
| **M1.5a — Artifacts** | The adapter-template engine in host and worker ([adapter-templates.md](adapter-templates.md)); Tier 0 adapters replacing per-site logging, with two or three starter adapters for tools the user opens daily; the worker prefilter from the policy lease; hourly activity totals, trails and refs, mode, and the one-liner; one intent per delivery batch and compact intents; record keys; `artifacts`/`visits`/`edges` index tables and lazily regenerated artifact files; the artifact card, the Recent tab, and the worklog view. No agent required |
| **M1.5b — Artifacts, continued** | Aliases for moved issues and renamed repositories, Tier 1 fields (which add `scripting`), worklog files, and the remaining starter adapters, after M1.5a has been in daily use for a few weeks. Tier 2 after M2 host invocation (§12.2 step 8.2). No agent required |
| **M2 — Agent** | Harness setup with native confirmation, content extraction, streaming with cancellation, `/summarize`, denylist enforcement |
| **M3 — Related** | FTS5 index, related pages, read later, `reindex` |
| **M4 — Omnibar** | Built-in commands, free text, custom commands |
| **M5 — Release** | Installers, store listings, signing and provenance, docs; Windows smoke run and directory-entry durability |

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
3. **Settle M2 design before code: done (2026-09-28).** The answers are in §14 and in §4.4, §5.2, §6, §7.1, §7.3, and §8. The last open question, Codex isolation, was answered on 2026-09-28: Codex runs with the user's normal home (§14).
4. **Rework the side panel and make notes editable (before M2): done (2026-09-28).** Built the §5.7 layout with the controls M1 already has: the page area following the active tab, the autosaving note (§5.3) with the host's versioned replace (§4.4), notes on any HTTP(S) page started from the toolbar action, a keyboard command, or the context menu (§5.3), which brings `activeTab`, `contextMenus`, and a `note` entry under `commands` (§8) forward from M2, and the capture strip collapsed into a one-line disclosure. This replaces the create-only note flow and review drafts. Protocol version 3 replaces `create_page_note`/`page_note_result` with `load_note` (returns `exists`, a `revision`, `title`, `body`) and `save_note` (takes `expected_revision`; a match replaces the note whole and returns `note_saved`, a stale revision returns `note_conflict` with the note's current content instead of writing anything). The host reuses `get_config`/`update_config`'s revision-check pattern; before replacing, it checks the existing file's recorded canonical URL and `brauser:` ownership (kind absent or `page`, never `summary`), never adopting a mismatch. The answer card, Summary section, and full omnibar remain M2/M3 and are not built; the omnibar's one-line input and the `/summarize`, `/related`, and `Read later` surfaces have no placeholder yet beyond the capture strip. The headless smoke run (`npm run smoke:macos:headless`) now exercises autosave (typing, then the debounce save; a save flushed by navigating the page away and back) and the refused-save path (a simulated second window saves first, so the panel's own save is refused, shows the current note, and keeps the unsaved text in a copy-out box; a later edit then saves cleanly against the now-current revision) in place of the removed create-only/review-draft checks. Not yet verified against real Chrome: whether the toolbar action alone (via `openPanelOnActionClick`, with no `action.onClicked` handler) grants `activeTab` the way the `note` command and context-menu item do — the same open question M2's build order already flags for `summarize` (§12.2 step 8.4).
5. **M2 step 1, harness setup: done (2026-09-28), merged in PR #19.** The host offers harnesses found on `PATH` and shows a native confirmation of the binary, arguments, environment variable names, the denylist review (§7.3), and the proposed `summaries_dir` (§6.1). As with `storage.root`, the extension never supplies a raw binary path; `update_config` requires a single-use token bound to the confirmed harness entry and denylist, and that token sets `agent_denylist_confirmed`. The same setup records the harness version and identity, checks every flag against `--help` (and `codex features list`), runs the one-token probe, and refuses a harness that fails (§7.1). Add optional `summaries_dir` and the privacy keys to the protocol `StorageConfig`/config schema, validation, and overlap checks, with a protocol version bump. Tests cover the host refusing agent requests while the denylist is unconfirmed and `update_config` rejecting an extension attempt to set it. See the as-built notes in §7.1. macOS and Windows CI pass, as does the headless smoke run on macOS; the smoke run covers it end to end with fake harnesses on a pinned `PATH` (detection, including a fake Codex passing its checks, a canceled setup, a failing probe, a confirmed setup, denylist add and removal, a confirmed Codex setup whose dialog carries the §14 disclosure and whose probe runs the reviewed argv with only `HOME` and `PATH`, and a denylist edit after the harness is removed). Not yet verified: the real macOS confirmation dialog (guided or `--auto` run), and setup against real `claude` or `codex` binaries. Where a Chrome-spawned host gets auth values is still open (§14).
6. **Build M1.5a (artifacts)** next, after M2 step 1 and before the rest of M2 (step 8), in this order, independent of M2 except where noted. Record keys (step 6.7) must land before M2 `/summarize` (step 8.5) writes any summary. It gives the product a clear first-release reason to exist ("Brauser remembers the tickets and docs you work on") without an agent. It ships with two or three starter adapters for tools the user opens daily, so the log is exercised on real work from the first day.
   1. Smoke-runner probes first: `webNavigation.onCreatedNavigationTarget` for a middle-click, a `target=_blank` link, and `window.open`, including a source tab that navigates on before the new tab commits; transition types for a link, a typed URL, and a board-modal `?selectedIssue=` change (`onHistoryStateUpdated`); `chrome.storage.session` and a pending `storage.local` activity total surviving a forced worker stop; and `idle.setDetectionInterval` at 5 minutes; and that switching to a discarded tab and restoring a session commit with transition type `reload`, so neither counts as an open (§5.1).
   2. Adapter-template engine, built to [adapter-templates.md](adapter-templates.md) as a pure library with no protocol, config, or UI change, so it can be reviewed and merged on its own. Before it starts, the spec moves from draft to accepted, and its open questions (§13) that change the grammar or title preparation, such as unread-count prefixes, are settled; the others can wait. Two independent implementations of the same grammar, neither using a regular-expression engine: a Rust crate added to the Cargo workspace (`adapter-templates/`) for the host, and a TypeScript module in `extension/` for the worker. Each covers parsing and validation (spec §4.1, §5, §6, §7, §8, §9, §10.3 limits), URL preparation (§5.1), path matching (§4.3), query conditions and modes (§5.3, §5.4), scope (§6), capture types and `@tail` (§7), `id` building (§8), title templates (§9), and refs (§10.1). The host crate also implements the conservative overlap check (§10.3) and the Tier 2 draft structure rules (§10.4), and the worker module needs only what the prefilter uses. A `brauser adapters eval` CLI subcommand evaluates a template against a URL and title for tests. Tests follow spec §12: shared vectors in `/protocol/adapter-templates/vectors.json` run by both implementations in CI, with the spec's §11 starter examples among them; differential property tests between the CLI and a Node harness, with near-miss inputs; `cargo-fuzz` targets for the parser, URL preparation, and title matching; bounds tests at every cap within a fixed time budget in both engines; and scope tests for each shared origin. They also cover a URL with `%2F`, `.`, or `..` in a segment, a title capture that disagrees with its path capture, a captured alternation containing `:`, and a scope value no URL could produce.
   3. Adapter config and lease: the `[[artifacts]]` schema and `[adapters] format = 1` in config, validated by the step 6.2 crate, with scope and the shared-origin list, overlap warnings, and the native confirmation (§7.2), and a bump to protocol v5 (M2 step 1 takes v4). The policy lease carries the confirmed adapter templates, and the worker prefilters with the step 6.2 module before buffering (§3.2). The host re-checks every delivered event and returns `suppressed`/`not_an_artifact` for any that does not match. Tests cover an unsupported `format` suspending policy without deleting anything, an `id` change refused on an adapter with recorded activity (§6.2), an adapter whose origin is not enabled, an unscoped shared-origin adapter whose confirmation carries the every-account statement, and a simulated compromised worker that delivers an unrecognized URL or a URL outside the adapter's rules or scope; neither may be recorded.
   4. Activity events: the worker's hourly totals (focused and background seconds, cold opens, modes, trail sources) frozen into `activity` events, plus `why` events. All are append-only and idempotent by event ID, like M1 visits. Tests cover an hour boundary, a UTC-offset change, a browser restart with a pending total, and idle and lock during a long read. Open-count tests cover repeated switches between two artifact tabs, switching to a discarded tab, a reload, a restored session, and a route change within one artifact, none of which is an open. They also cover a board-modal switch between two issues and the same artifact opened in two tabs, each of which is.
   5. Delivery and durability: one write, one sync, and one intent record per delivery batch, and the compact intent index brought forward from M5, replacing M1's one intent file per visit, with buffer-pressure coalescing, the 80% badge, and overflow accounting shown in the strip and marked in the log (§5.1). Retire `near_repeat_secs` (§5.1): no artifact event or `/log` entry is near-repeat suppressed, the host accepts and ignores the key in an older config, protocol v5 drops it from the config schema, and the "Shorten repeat suppression" confirmation line goes with it. Tests cover two `/log` entries for one URL a minute apart, both recorded.
   6. Index tables and artifact files: `artifacts`, `visits`, and `edges` in `index.db`; lazy regeneration from the dirty set, skip-if-unchanged, and the Timeline rollups (§6.7); `brauser reindex` rebuilding both from the logs. Tests cover a hostile title (embeds and YAML breakers), a case-colliding ID, an external edit to an artifact file being replaced, a day of heavy use producing no file write until the regeneration pass, and a year of daily opens staying within the rollup bounds.
   7. Panel: the artifact card (§5.7), the one-liner, the Recent tab, and the worklog view with Copy (§5.1), with `get_artifact`, `list_artifacts`, and `worklog` messages. Record keys (§6.2) land here, before M2 writes any summary: `load_note` and `save_note` resolve and return the key, and the panel lists older URL-keyed notes. Tests cover a Google Doc's `/edit` and `/view` sharing one note, a board modal and `/browse/` sharing one note, a `key_changed` refusal after an adapter change mid-edit, **Use as artifact note** refusing when an artifact note exists, and an M1 note loading unchanged.
   8. The first two or three starter adapters, written in adapter templates and each checked against the live tool on the date it records, with the recorded URLs and titles added to the step 6.2 vectors, plus the sync-location warning.
7. **Build M1.5b (artifacts, continued)** once M1.5a has been in daily use for a few weeks. Moved issues and renamed repositories are rare, so aliases wait until there is a real log to judge them against. M1.5b can run alongside the rest of M2 (step 8); if it adds messages, it takes the next protocol version.
   1. Alias probes: that `onBeforeNavigate` plus a committed `server_redirect` qualifier pairs the requested and final URLs for a moved Jira issue and a renamed GitHub repository. If Jira's move redirect turns out to be client-side, only the title signal is used for it.
   2. Aliases: `alias` and `unalias` events, append-only and idempotent by event ID, and notes reached through an alias listed on the artifact. Tests cover a chain (A→B→C), a move back, an `unalias`, a stub left at the old path, a ref to an old key, a board-modal switch between two issues that must not create an alias, a compromised worker's alias between IDs no adapter produces, and notes on both sides of an alias.
   3. Tier 1 fields in the worker while the page is live. This step adds `scripting` to the manifest (§8); M2 content extraction (step 8.4) reuses it. The policy lease carries the confirmed Tier 1 selectors, and the host drops any undeclared Tier 1 field a compromised worker delivers. The smoke run checks that fields are captured with the panel closed, including a late-rendering SPA field that needs the retry.
   4. Worklog files: the optional Brauser-owned daily or weekly files (§5.1), off by default, behind the sync caution.
   5. The remaining starter adapters for the common tools, each checked against the live tool on the date it records.
   6. Tier 2 drafting, after M2 host invocation (step 8.2), using the same harness isolation and the URL-shape sample in §5.1. Shaper tests feed real URL forms from every shipped starter adapter, plus Confluence `/display/SPACE/Title`, SharePoint `sourcedoc` GUIDs, percent-encoded names, and long mixed-case slugs, and check that no stored shape contains a digit, an uppercase letter, a segment over 20 characters, a query value, or any title. Draft tests use planted URL paths that steer toward `/{*}`, a root match, a search-page match, an ID-less or empty-capture draft, an ID taken from a search parameter, a draft carrying Tier 1 fields, a title template, or a `scope`, a draft on a shared origin with no scoped adapter, a draft overlapping an existing adapter, and a draft that does not parse; the host must reject each before the preview, and the preview must list every non-artifact shape a borderline draft matches.
8. **Build the rest of M2** after M1.5a, in this order, each step reviewed and merged separately. The sub-steps keep M2's step numbers, so step 8.2 is M2 step 2.
   2. Host invocation: an environment of only the allowlisted variables, argv from the template, page content on stdin inside the untrusted-data block, the host-owned working directory, and a timeout, output cap, and process-tree kill. Test with fake harness binaries on both OSes, including one whose symlink target changes between runs (identity re-check), one that emits a tool call or an unknown Codex event, and one that reports `is_error`. Run the §7.1 canary for each shipped template, including a prompt-injected page that asks Codex to read a planted file; it passes if the contents appear in no forwarded chunk or summary and any non-allowlisted event ends the run as `rejected`.
   3. Streaming protocol: the host currently answers one request at a time, so it needs a reader that stays responsive while a harness runs. Add chunk, `rejected`, error, `cancel`, and `summary_result` messages correlated by `request_id`, each under the 1 MB native-messaging limit (§5.2). Closing the panel (stdin EOF) kills the harness.
   4. Content extraction with a bundled, pinned Readability-style library, the `summarize` command, context menu, and toolbar trigger, and the permissions in §8 (`scripting` is already present from M1.5b). First confirm with the real-Chrome smoke runner that (a) the command, context-menu, and toolbar-action paths grant `activeTab` to `chrome.scripting.executeScript`, and whether opening the panel from the action (`openPanelOnActionClick`) grants it; (b) the grant ends when the tab navigates; (c) the panel sees no URL for an ungranted tab; (d) the grant does not carry over to another tab in the same window.
   5. `/summarize` in the panel, with denylist checks and the summary write and result contract in §5.2 and §4.4. Test an external-editor race, a Windows rename while another process holds the file open, a shared or overlapping summaries directory, and a config change during a fake-harness run, which must discard the output.
9. **M3–M5** follow as in the table. The Windows smoke run and Windows directory-entry durability are release blockers tracked under M5; the compact intent index moved into M1.5a (step 6.5).

## 13. Platforms and License

- **Browsers:** Google Chrome Stable only; see the release support window in §11. Edge and all other browsers are out of scope for the first release.
- **Operating systems:** macOS 15 Sequoia and later on Intel or Apple silicon; Windows 11 version 25H2 and later supported releases on x64. Windows 10, Windows on ARM, and Linux are out of scope for the first release.
- **License:** Apache-2.0, chosen for its explicit patent grant.

## 14. Open Questions

No open M0 or M1 design questions. The release hosting, support, platform, browser, and signing policies are defined in §11; M1 security requirements are recorded in §12.

Artifact decisions (2026-09-28):

- **Sequencing.** M2 step 1 (harness setup, protocol v4) finishes first, then M1.5a (protocol v5), then M2 steps 2–5 (§12.2 step 8). M1.5a opens with the adapter-template engine (step 6.2, built to adapter-templates.md), which changes no protocol and can be built while harness setup is still under review. Harness setup is already underway, and M1.5a needs no agent. M1.5b follows once M1.5a has been in daily use for a few weeks and can run alongside M2 steps 2–5; Tier 2 drafting (step 7.6) waits for M2 host invocation (step 8.2). `scripting` arrives with Tier 1 in M1.5b (step 7.3), and M2 content extraction (step 8.4) reuses it.
- **M1.5 is split.** M1.5a is Tier 0 with the lease prefilter, activity totals, batch intents, record keys (which must land before summaries exist), the artifact card, and Recent, with two or three starter adapters the user opens daily. M1.5b is aliases, Tier 1, worklog files, and the remaining starter adapters. Moved issues and renamed repositories are rare, so aliases can wait until the log has been lived with for a few weeks.
- **Artifacts replace per-site logging.** Automatic capture records only URLs an enabled adapter recognizes as an artifact (§5.1, §7.2). Per-site "log every page" is not offered alongside it. `/log` remains the explicit one-off exception.
- **Log is the source of truth; artifact files are derived.** The daily log stays append-only. Artifact files and the index tables are rebuildable from it, so replacing an artifact file whole is safe under the same ownership rules as summaries (§4.4).
- **Measured, not inferred.** Attention is recorded as focused and background minutes, with no "glanced" or "worked on" labels. The idle detection interval is 5 minutes, so a long read without input still counts (§5.1).
- **Recognition in the worker.** The policy lease carries the confirmed adapter rules and Tier 1 selectors, so the worker prefilters and reads fields while the page is live. The host re-checks every event, so the worker can only narrow what is recorded (§3.2).
- **Opens are cold opens.** An open is a navigation that makes a tab show an artifact it was not showing. Tab switches, focus changes, and reloads (including Chrome's reload of a discarded tab) are not opens and are not logged; they only move focused and background time (§5.1).
- **Coarse log, lazy files.** The worker merges each artifact's activity per hour, the host writes one sync and one intent per delivery batch, and artifact files regenerate lazily with bounded Timelines (§5.1, §6.7).
- **No near-repeat window.** M1.5a retires `near_repeat_secs`. Hourly totals already bound artifact lines, and a `/log` entry is deliberate, so nothing is suppressed as a near repeat; replays are absorbed by event ID (§5.1).
- **Canonical ID plus aliases.** IDs change when issues move and repositories are renamed. Server redirects and declared identity signals record `alias` events, the index resolves them transitively, and `unalias` undoes a wrong one (§5.1).
- **Trails at creation time.** New-tab edges come from `onCreatedNavigationTarget` and the worker's tab-to-current-artifact map, not from reading the opener's URL later (§5.1).
- **Recent and the worklog view ship with M1.5a.** A cross-tool Recent list and a "what did I do yesterday" worklog view with Copy, so the milestone is useful without the M3/M4 queries; worklog files wait for M1.5b (§5.1).
- **Mode, not edits.** The URL's mode is recorded as seen; Brauser does not claim the user edited something.
- **Record keys.** Every per-page record (notes, summaries, read-later entries) is keyed by `artifact_id` on a recognized page and by URL otherwise, derived by the host alone. URL-keyed notes stay in place and are listed on the artifact; nothing is migrated automatically (§6.2).
- **URL shapes, not URLs, for Tier 2.** The unrecognized-URL sample stores only URL shapes, with identifiers and title-like text replaced by typed placeholders and no titles, for example `/wiki/{num}/view`. That is enough to draft an adapter, keeps no record of what the user browsed, and removes the prompt-injection path through titles. The sample is on by default (§5.1).
- **Adapter templates, not regular expressions.** Adapters are written in the declarative language in [adapter-templates.md](adapter-templates.md), format version 1, with no raw-regex escape hatch. It is linear in both engines by construction, readable by a reviewer, and lets the host check draft structure and overlap mechanically, which replaces the two-engine regex dialect, its host-side rewrite, and most of the Tier 2 regex rejection rules (§7.2, §5.1). The shipped adapters for common tools are now called **starter adapters**. Shared origins need a `scope` or an explicit `unscoped` acknowledgment (§7.2). Title and meta aliases are suggested until confirmed or corroborated by a redirect (§5.1).

Open artifact questions:

- **ID scheme alignment.** If a user's other tools already key these artifacts (for example AMOS's Jira and Confluence ingestion), should the starter adapters' `id` templates match that scheme exactly? AMOS already made the same canonical-ID-plus-aliases decision, which strengthens the case for one shared scheme and for alias events Brauser records being usable by those tools too. Confirm the target scheme before starter adapters ship in step 6.8; a starter adapter's `id` template is frozen once it ships (§6.2).
- **Adapter-template open questions** are tracked in adapter-templates.md §13: hash-routed apps, Notion pages with no workspace segment under a scoped adapter, unread-count title prefixes such as `(3) `, and which further origins join the shared-origin list at release.

M2 decisions (2026-09-28):

- **Extraction permissions.** Any page can be summarized. A side-panel click does not grant `activeTab`: Chrome's documentation lists only the action click, context menus, keyboard commands, and the omnibox, and Chromium withholds tab permissions from the side panel. Without `tabs`, the panel also cannot see an ungranted tab's URL, so it has no origin to request. Summarizing an ungranted page therefore starts from a trigger that grants `activeTab`: the `summarize` keyboard command, the page context menu, or the toolbar action. The panel's button works when the origin is already granted or an `activeTab` grant is present. M2 adds `activeTab` and `contextMenus` and does not add `tabs` (§5.2, §8). This refines the earlier "prompt only if needed" answer: no Chrome prompt is needed, and no persistent grant is made for summarizing.
- **Summary destination.** A summary is a separate record from the page note: one Brauser-owned summary file per page, `brauser.kind: summary`, in its own non-overlapping `summaries_dir`, replaced whole on each new summary (user edits to it are replaced), linking to the page and its note (§5.2). The note holds only the user's own text. Because summaries never touch notes, M2 does not need the managed-block replacement in §6.4; the one replace primitive it needs, a checked rename limited to summary files, is in §4.4.
- **Harness modes.** Checked against the Claude Code 2.1.283 and codex-cli 0.144.4 documentation, help output, and trial runs (§7.1). Claude Code can run with no tools and is the recommended harness. Codex can run with its shell, web search, and user config disabled in a read-only sandbox, but documents no switch that removes every tool, so the host accepts only an allowlist of Codex event types and aborts on anything else.
- **Denylist setup.** Required once: the first harness setup's native confirmation shows the agent denylist with its suggested categories, and the host refuses every AI command until that confirmation sets `agent_denylist_confirmed`, even if the list is empty (§7.3).
- **Codex isolation.** Codex ships in M2 running with the user's normal `HOME` and `CODEX_HOME`. A host-owned `CODEX_HOME` would keep the user's global `AGENTS.md` and skills out, but its copied credential could, on refresh, break or corrupt the user's own Codex login. Sharing the normal home avoids that. The accepted cost is that user-level Codex instructions and skills can shape Brauser's output. The Codex harness confirmation, the settings page, and the user documentation state this plainly. Tool use is still limited by the read-only sandbox, the disabled features, and the event allowlist (§7.1).

Open M2 question (2026-09-28):

- **Where a Chrome-spawned host gets auth values.** Chrome starts the host with the environment Chrome itself inherited, usually launchd's on macOS, not a login shell's. A Bedrock, Vertex, or API-key user's variables are then often absent, and setup can only report them as not set. Options: tell the user to set them where Chrome inherits them (for example `launchctl setenv`), read a host-owned env file the user edits, or let the harness use its own stored login where it has one. Undecided; step 1 passes through only names the host process already has.

Side panel decisions (2026-09-28):

- **Editable notes.** A page note is a scratch space the user can add to at any time, edited only in the side panel and autosaved as they type (§5.3). This replaces M1's create-only notes and review drafts. The host replaces the note whole and refuses a save based on an old version (§4.4).
- **No AI output in notes.** Commands send output to the sidebar, a new separate file, or the clipboard (§5.6). The `page_note` output is removed.
- **Notes on any page.** Notes work on any HTTP(S) page; the first note on a new site starts from the toolbar action, a keyboard command, or the context menu, because the panel cannot see an ungranted tab's URL; the panel then requests the site's origin from Chrome (§5.3).
- **`/log` on any page.** `/log` logs the current page now, even on a site not set up for logging, without enabling automatic logging there (§5.6).
- **No native confirmation for one-off actions.** Writing a note and running `/log` need only Chrome's site grant (§7). The trade-off: a compromised extension could write a note or log entry for any single page, where before it could do so only on sites the user confirmed. It still cannot enable automatic logging, change the notes folder, or weaken privacy settings without the host's confirmation.
