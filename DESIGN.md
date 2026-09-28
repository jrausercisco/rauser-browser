# Rauser Browser Browsing Assistant — Design Specification

> Status: Draft v0.5 · M0 merged, M1 accepted for development on macOS, no public package yet · Short name "Brauser"; CLI and binary `brauser` · License: Apache-2.0 · Platforms: Google Chrome on macOS and Windows

## 1. Purpose

Brauser turns the browser into a place where context accumulates instead of disappearing. It runs as an always-on sidebar that:

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
4. **Never clobber user edits.** Brauser only modifies what it owns.
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
| Prompt injection from page content | Content delimited and labelled untrusted; agent run in its most restricted mode for read-only tasks; outputs written only to Brauser-owned locations |
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
- Page notes are edited only in the side panel (§5.3), so the host replaces a note whole: write a temp file in the same directory, fsync it, then atomically rename it over the note. Each save names the note version it was based on; if the file has changed since then (for example, from another Brauser window), the host refuses the save and returns the current note. Before replacing, the host checks the file's recorded canonical URL and Brauser ownership, as for summaries (§5.2).
- Destructive operations, such as deleting a note or read-later file, require explicit user confirmation in the sidebar. Saving a note's text is not destructive.

### 4.5 Agent invocation rules

- Each harness is a config entry: absolute binary path, argument template, and environment allowlist.
- Spawned with a cleared environment plus allowlisted variables, a timeout, an output size cap, and cancellation support.
- Page content goes on stdin inside a delimited block with an explicit instruction that it is untrusted data.
- For summarization and Q&A, harnesses should be configured in a mode without shell or file-writing tools. The host writes results itself; the agent never writes into the vault directly.
- The user's agent denylist (§7.3) blocks content from those domains from ever reaching an agent. The host checks it before the extension extracts content and again before spawning the harness.

## 5. Features

### 5.1 Site logging

- Logging is **opt-in**: the user enables a site and grants its exact origin from a setup action. The host independently checks scheme, host, and any adapter path rule before recording a visit; Chrome's origin permission does not enforce URL paths.
- Incognito and private windows are never logged, regardless of config. The extension manifest uses `incognito: "not_allowed"`, and the event handler resolves the tab's incognito state and rejects it defensively.
- M1 observes main-frame `webNavigation.onCommitted` and `onHistoryStateUpdated` events, using document identity and lifecycle to avoid subframes, redirects, and duplicate SPA records. It does not keep dwell or scroll timers only in service-worker memory. A committed document's title arrives after commit, so the worker briefly holds a new untitled visit and fills its title from the tab's title update; the event is frozen once handed to the panel because the host hashes it for interrupted-write recovery. Each visit carries its local UTC offset, and the daily log uses that local calendar date.
- M1 normalization drops fragments and configured tracking parameters after host authorization. Later opt-in site adapters may apply site-specific transforms (for example, collapsing a GitHub file view to its repository).
- Deliveries are idempotent by event ID recorded in the Markdown log. The host keeps a durable per-event intent in its OS config directory to find the original daily log across date and `log_dir` changes; only a complete Markdown marker proves persistence. Shared locks cover the current policy check, intent, append, and file sync, so revocation and concurrent panel connections cannot race a write. An interrupted append can resume only from an exact byte prefix in the still-selected folder; changed or missing logs require review. Near-repeat visits within a configurable window are suppressed rather than rewriting a prior Markdown entry; the daily log remains append-only. Dwell-time and scroll-depth filters follow M1 only after their state can survive service-worker suspension.

### 5.2 Summaries

1. The extension extracts readable content (Readability-style) from the active tab on demand. Any HTTP(S) page can be summarized. A side-panel click does not grant `activeTab` (§8), so when the extension does not already hold the page's exact origin, the panel's summarize click requests it, and Chrome prompts once for that site. A grant made for summarizing does not enable logging; the host's site rules still decide what is logged.
2. The host checks the denylist, then invokes the configured harness with the summary prompt.
3. Output streams back to the sidebar in chunks, staying under the 1 MB native messaging limit per message.
4. The host writes the final summary itself, to a summary file that is a separate record from the page note (§6.1). The note holds only the user's own text; Brauser never writes a summary into it. Each page has one summary file, keyed by the same URL-derived identity as its page note. The summary records the page URL and links to the page note. Summarizing the page again replaces the whole file atomically. Summary files belong to Brauser, and user edits to them are not preserved. Before replacing, the host checks the file's recorded canonical URL and Brauser ownership; a file that fails the check is a conflict and is never overwritten.

### 5.3 Page notes

- A page note is a persistent scratch space attached to the page. The user can add to it at any time; when they return to the page, the panel shows it again.
- One markdown file per normalized URL. It is created on the first keystroke, not when a page is opened, so visiting a page never creates an empty note.
- Notes work on any HTTP(S) page, not only logged sites. The first note on a site requests that exact origin from Chrome, as summaries do (§5.2), so the panel can read the tab's URL and title. A grant made for a note does not enable logging.
- The note's title comes from the page's title when the note is created (or its host name when the page has none); the panel asks only for the note text.
- The panel autosaves: it sends the whole note about one second after the user stops typing, and again when the active tab changes or the panel closes. There is no Save button. A small indicator shows "Saving…", "Saved", or an error.
- The side panel is the only supported editor. The files stay plain Markdown and can be read anywhere, but an edit made outside Brauser can be overwritten. The version check in §4.4 exists so two Brauser windows do not overwrite each other: when a save is refused, the panel loads the newer note and shows the text it could not save so the user can copy it back.
- A page note holds only the user's thoughts. Agent output never goes into a note; it goes to the page's separate summary file (§5.2) or wherever a command sends it (§5.6).
- Notes created by M1 load into the editor normally. M1 review drafts are left in place and are not merged.

### 5.4 Read later

- `/save` writes a read-later entry with an optional agent summary.
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

The user chooses the notes folder at setup. It can be empty, or a subfolder of an existing knowledge base. The three content locations are configurable; the layout below is the suggested starting point offered at setup, not a requirement.

```
<notes folder>/                  chosen by the user, no default
  log/2026/09/2026-09-27.md      daily browse log
  pages/<slug>.md                page note: the user's text only
  summaries/<slug>.md            one Brauser-owned summary per page (M2)
  later/<slug>.md                read-later items
```

Brauser's own files live outside the notes folder, in standard OS locations, so it never adds hidden folders to someone's knowledge base:

| File | Location |
|---|---|
| `config.toml` | OS config directory (e.g. `~/Library/Application Support/Brauser` on macOS, `%APPDATA%\Brauser` on Windows) |
| `visit-ids/` | OS config directory; M1 keeps one synced intent file per persisted visit |
| `index.db` | OS cache directory |

If Brauser finds an existing folder with files it didn't create, it leaves them alone. It only indexes files under its configured content locations.

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

**Reading is lenient, writing follows the profile.** The indexer parses both link styles and any frontmatter format. Switching profiles never rewrites existing files; `brauser migrate --profile <name>` converts them and defaults to a dry run.

### 6.4 Managed blocks

Brauser owns only:

- Its own frontmatter keys, namespaced under `brauser:`
- Content between `<!-- brauser:start -->` and `<!-- brauser:end -->`

Page notes no longer use managed blocks: the side panel is their only editor, so the host writes the whole file (§4.4, §5.3), and summaries live in their own files (§5.2). Managed blocks remain the rule for any later feature that writes into a file the user also edits elsewhere. Such replacement is deferred until a cross-platform atomic displaced-byte backup and conflict procedure is proven with an external-editor race test.

Page-note filenames include a stable identifier derived from the normalized canonical URL, even when a profile uses a title slug. Before updating a file, the host verifies the recorded canonical URL and Brauser ownership match the requested page. An absent or different identity is a conflict, never an opportunity to adopt or overwrite an unrelated file.

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
        "--max-turns", "1", "--no-session-persistence", "--output-format", "stream-json",
        "--verbose", "--include-partial-messages"]
env_allow = ["HOME", "PATH"]
timeout_secs = 120

[agent.harnesses.codex]
binary = "/usr/local/bin/codex"
args = ["exec", "--sandbox", "read-only", "--ignore-user-config", "--ignore-rules",
        "--ephemeral", "--skip-git-repo-check", "--json",
        "-c", "approval_policy=\"never\"", "-c", "web_search=\"disabled\"",
        "--disable", "shell_tool", "--disable", "unified_exec", "--disable", "apps",
        "--disable", "plugins", "--disable", "multi_agent", "--disable", "hooks",
        "--disable", "memories", "--disable", "browser_use", "--disable", "computer_use",
        "--disable", "image_generation", "{prompt}"]

[agent.harnesses.generic]
binary = "/path/to/any-cli"
args = ["{prompt}"]
stdin = "page"
```

`{prompt}` is the command prompt only. Page content always goes on stdin, never into argv.

The argument lists above follow the Claude Code 2.1.283 and codex-cli 0.144.4 documentation and help output (checked 2026-09-28) but have not yet been run. Each harness starts in a new empty temporary directory, so no project instructions or project config load. Claude Code with `--tools ""` and `--disallowedTools "*"` has no tools; if `--setting-sources ""` is rejected, `--safe-mode` also stops CLAUDE.md, hooks, MCP servers, and plugins from loading. Administrator-managed settings still apply. Codex documents no switch that removes every tool, and its feature names change between versions, so the host rejects any Codex run whose JSON event stream contains a tool call. At setup, the host records each harness's version, checks its `--help` output (and `codex features list`) for every flag it will pass, and refuses a harness that lacks one. A non-zero exit fails the run. Prompt injection in page content can still distort a summary; it cannot run commands or write files.

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

The denylist starts empty rather than shipping a guess at what the user considers sensitive. The first harness setup shows the denylist and offers suggested categories (banking, email, HR, health) as examples to adapt. No AI command runs until the user confirms the list once, even if they leave it empty.

## 8. Extension Permissions

| Permission | Why |
|---|---|
| `sidePanel` | Always-on sidebar |
| `nativeMessaging` | Talk to the host |
| `storage` | UI preferences and the capped visit buffer (§3.2) |
| `webNavigation` | Optional in M1; requested only when the user enables logging |
| `scripting` | Optional, M2; runs the pinned content extractor in a tab whose exact origin is granted |
| `optional_host_permissions` | Manifest declares HTTP(S) patterns for sites discovered during setup; Chrome grants only the exact origin requested for an enabled site. Adapter paths remain host-enforced |

The manifest's broad optional HTTP(S) patterns permit runtime requests for user-chosen sites; no origin is granted at install time. The permission request runs directly from a settings-page button while Chrome still has the user gesture, before awaiting native confirmation. If native confirmation is canceled, the extension removes any newly granted origin and optional API permission. Removing a grant stops capture for that origin and purges its buffered events. The service worker reads the title from tab metadata on granted sites; M1 requests neither `scripting` nor `activeTab`. M2 adds `scripting` for content extraction. It does not rely on `activeTab`, because a click in the side panel does not grant it; the summarize click, the first note on a site, and `/log` request the page's exact origin instead (§5.2, §5.3, §5.6).

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

- **Download experience:** provide one public Brauser download page with two clearly labeled steps: install the browser extension from the Chrome Web Store, then download and run the signed native-host installer for the user's OS. The page explains why both parts are needed, links to supported-browser instructions, and offers a short troubleshooting path. The native host is a companion installer; do not ask users to build from source, use a terminal, or install Homebrew to get started.
- **Extension:** publish the stable release through the Chrome Web Store. Keep the published extension ID stable because the native-host manifest authorizes that exact ID. Chrome Web Store installation is the supported path on Windows and macOS; do not instruct users to install a local CRX.
- **Supported browser:** Google Chrome Stable only, on macOS and Windows. Test the current and previous two stable major versions and publish the tested range on the download page. Edge, Brave, Arc, other Chromium browsers, Firefox, and Safari are unsupported in the first release. Chrome's Side Panel API requires Chrome 114 or later; the release floor must also satisfy the current three-version support window.
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

Before M1 exposes vault writes, the host must bind the notes root to a user selection made in a native OS folder picker. The current M0 `update_config` message alone does not prove user intent. M1 uses create-only page notes; an existing-file edit becomes a separate review artifact. Automatic replacement awaits a cross-platform displaced-byte backup design that survives an external-editor race.

### 12.1 M1 implementation and acceptance plan

1. **Prove setup authority before capture.** The settings page opens a persistent native connection and asks the host to show an OS folder picker. On macOS this uses a directory-only `NSOpenPanel`; on Windows the M1 host opens the Common Item Dialog without a parent window. Chrome supplies `--parent-window=0` when a service worker starts the host. Parenting to a nonzero Chrome window handle requires a separately audited adapter and Windows validation of handle lifetime and focus. Each synchronous picker or confirmation runs on the main thread of a short-lived child host process, which pauses the native connection until the dialog closes and then exits. A long-lived host that showed an AppKit modal and then blocked on stdin left macOS with a persistent busy cursor during the first smoke run; a fresh child also gives each Windows dialog its own COM apartment. The child only reports the user's choice; the parent validates the result and mints every token. The host canonicalizes the result and returns a short-lived, single-use selection token bound to the folder's filesystem identity. A config update changing or repairing `storage.root` must present that token and the current config revision; a raw path alone is rejected. Cancellation and stale tokens leave config unchanged. The host gives the panel a repair path for malformed, newer, or identity-less config files without performing capture. Exercise the picker and thread model on both OSes before M1 acceptance.
2. **Make consent and permission state explicit.** The MV3 manifest declares `incognito: "not_allowed"` and broad *optional* HTTP(S) match patterns, with no site origin granted at install. A setup button requests optional `webNavigation` and the chosen exact origin directly from the live Chrome user gesture, before awaiting the host; path-specific adapter rules remain host-enforced. The host shows its own native confirmation for every privacy-expanding config change and issues a single-use token bound to the exact change and revision. If that confirmation is canceled, remove the newly granted Chrome permissions and keep capture off. Only after host confirmation can the settings page enable capture. The service worker caches the host-confirmed policy with its revision and bounded expiry, fails closed if absent or expired, and awaits trusted-only `storage.local` access before buffering. Removing a grant or disabling a site stops capture immediately and clears its pending records. The host also disables a revoked site when informed, while acknowledging that it cannot inspect Chrome's live grant state. The service worker checks the incognito flag before buffering.
3. **Deliver visits across service-worker and panel lifecycles.** Capture only main-frame, active-document navigation and SPA URL changes for granted sites. The service worker stores a count- and byte-capped queue with stable event IDs and original URLs in `chrome.storage.local`, restricted to trusted extension contexts. It never relies on an in-memory dwell timer. On the next panel connection it sends pending records; the host authorizes the original URL against its allowlist and path rule before normalization. It returns a terminal per-event outcome: `persisted`, `suppressed`, or permanent-policy `rejected` with a reason. Transient vault/host errors (including an unavailable folder or failed durability step) return a retryable outcome; the event stays queued with bounded backoff and a visible error. The extension removes only terminally acknowledged records and reports rejections and overflow in the panel. Complete Markdown event markers are the persistence proof; the OS config intent locates the original log on replay.
4. **Write recoverable Markdown.** Normalize only HTTP(S) URLs, reject credentials, and drop fragments and configured tracking parameters without erasing meaningful query data after authorization. Site-specific transforms follow M1 as separately consented adapter rules. Under the config and capture locks, recheck the current host policy, sync an event intent outside the notes folder, append accepted visits with IDs to `log/YYYY/MM/YYYY-MM-DD.md`, and sync before acknowledging persistence. Retry completes only a byte-exact interrupted prefix in the currently selected root; it never overwrites unrelated user edits. The near-repeat window suppresses redundant entries without rewriting user text. Create page notes using a mandatory URL-derived identity suffix and record the canonical URL. Verify identity and ownership before treating an existing note as already present. Never replace an existing note in M1: write any proposed edit as a no-clobber sibling review artifact keyed by the proposed content, so a replay reuses the same draft. Malformed markers are conflicts. The UI must distinguish created, already present, conflict, and created-with-warning outcomes.
5. **Verify the complete no-agent path on both OSes.** CI covers normalization, permission and allowlist enforcement, incognito rejection, replay/idempotence including concurrent host processes, transient retry, traversal, and create-only page ownership/conflicts on macOS and Windows. A real Chrome smoke run with an unpacked development extension and development-only native-host registration checks first-run setup, panel close/reopen, visit replay, permission removal, picker cancellation, and page-note creation. No release claim or installer download link is made during M1.

The plan follows Chrome's current [optional-permission rules](https://developer.chrome.com/docs/extensions/reference/api/permissions), [incognito behavior](https://developer.chrome.com/docs/extensions/reference/manifest/incognito), [service-worker lifecycle](https://developer.chrome.com/docs/extensions/develop/concepts/service-workers/lifecycle), [storage access levels](https://developer.chrome.com/docs/extensions/reference/api/storage/StorageArea), and [native-messaging window handle](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging) contract. Folder picker details follow [Apple's `NSOpenPanel`](https://developer.apple.com/documentation/appkit/nsopenpanel) and [Microsoft's Common Item Dialog](https://learn.microsoft.com/en-us/windows/win32/shell/common-file-dialog).

**M1 validation remaining:** The macOS guided run loaded the unpacked extension, completed a Chrome-to-host `hello`, and exercised picker cancellation with config unchanged. The full permission, consent, visit, page-note, and removal flow was stopped before completion after a persistent macOS spinning cursor appeared; it is not a smoke-test pass. Native dialogs now run in a short-lived child process to remove that cause, which still needs confirming in a repeat macOS run; then run the flow on Windows. macOS and Windows CI build and test the branch. A failed append now rolls back its own bytes, and replay tolerates complete visits appended after an intent, so only a crash mid-append followed by further visits still requires manual repair. Confirm the Chrome permission request accepts explicit default and nondefault ports on both operating systems. Automated tests cover host identity/replay recovery and extension repair, pause, policy refresh, re-enable races, title capture, and notice retention. The M1 per-event intent journal grows indefinitely and needs a compact durable replacement before general distribution. Windows currently syncs file content but has no separately validated directory-entry flush, so power-loss durability of a newly created daily log or page-note name remains unverified. Resolve these gaps before the public package is released.

**macOS smoke pass (2026-09-28):** `npm run smoke:macos` completed the full flow on macOS with Chrome 154: first-run warning, picker cancellation, declined consent (with the new Chrome grants rolled back), confirmed setup, allowed and blocked visit replay, panel reopen without duplication, page note creation, identical retry, one review draft and its retry, site removal with Chrome grants removed, and no capture after removal. No busy cursor persisted after any native dialog, which confirms the child-process dialog fix. The fixture's nondefault port was accepted by Chrome's permission request. Re-requesting access that Brauser had just removed did not show a second Chrome prompt, so the host's native confirmation, not Chrome's prompt, is the gate for that re-enable; the host still required it. The runner now drives the extension pages itself through the DevTools pipe and stops only for the picker, Chrome's prompt, and the host dialog.

**macOS default port and unattended run (2026-09-28):** `node scripts/smoke-macos.mjs --default-port --auto` passed the full flow with the fixture on port 80 and the site entered as `http://127.0.0.1:80`. Chrome granted exactly `http://127.0.0.1:80/*` and not the any-port pattern, and the host stored the origin without the port. A repeat run on a nondefault port also passed. `--auto` answers every prompt through macOS UI scripting, so the run needs no person once the terminal has Accessibility access. The host's folder picker does not answer Accessibility queries and is driven by keyboard. Its Yes/No alert is drawn by `UserNotificationCenter`, and Chrome's permission prompt is a sheet on the settings window, so both buttons are pressed by name. In place of the busy-cursor question, the runner checks that no dialog process or host window remains. **M1 is accepted for development on macOS.**

**Page note title (2026-09-28):** The panel's page-note form no longer has a title field. The host receives the tab's title, or the host name when the page has none; a change in that title still produces a review draft.

**Settings page (2026-09-28):** Configuration moved from the side panel to a dedicated `options_ui` page. The panel shows a gear button and a setup warning until a folder, a site, and capture are all in place. The page opened from the panel on macOS once Brauser was reloaded in `chrome://extensions`. Chrome keeps an unpacked extension's manifest and service worker from load time but serves rebuilt pages from disk, so a build that changes `manifest.json` or `worker.ts` needs that reload; the panel now says so when the settings page cannot open. `npm run smoke:macos` starts a fresh profile and is unaffected. This is not a smoke-test pass for the flow above.

### 12.2 Next steps

1. **M1 acceptance on macOS: done (2026-09-28).** The full run passed on both a nondefault and the default port (above).
   - **Windows parked (2026-09-28).** No Windows machine is available, so the Windows smoke runner and its interactive run are deferred. Windows CI continues to build and test every PR. Porting the runner (Chrome path, native-host registration in the registry rather than the profile, and no `pbcopy`), the Windows run, and its default-port check are release blockers tracked under M5.
2. **Catch page-load regressions in CI (optional, recommended).** Unit tests import the pages but do not load the built extension. A headless Chrome step that loads `extension/dist/` and opens the panel and settings page would have caught the stale-manifest class of failure before a manual run.
3. **Settle M2 design before code: done (2026-09-28).** The answers are in §14 and in §5.2, §6.1, §7.1, §7.3, and §8.
4. **Rework the side panel and make notes editable (before M2).** Build the §5.7 layout with the controls M1 already has: the page area following the active tab, the autosaving note (§5.3) with the host's versioned replace (§4.4), notes on any HTTP(S) page, and the capture strip. Add a host message that loads a page's note so the panel can show it again. This replaces the create-only note flow and review drafts; update the smoke runner for autosave and the refused-save path.
5. **Build M2 in this order**, each step reviewed and merged separately:
   1. Harness setup: the host offers harnesses found on `PATH` and shows a native confirmation of the binary, arguments, and environment allowlist. As with `storage.root`, the extension never supplies a raw binary path; `update_config` requires a single-use token bound to the confirmed harness entry.
   2. Host invocation: a cleared environment, argv from the template, page content on stdin inside the untrusted-data block, and a timeout, output cap, and process-tree kill. Test with a fake harness binary on both OSes.
   3. Streaming protocol: the host currently answers one request at a time, so it needs a reader that stays responsive while a harness runs. Add chunk, completion, error, and `cancel` messages correlated by `request_id`, each under the 1 MB native-messaging limit. Closing the panel (stdin EOF) kills the harness.
   4. Content extraction with a bundled, pinned Readability-style library, and the permissions in §8. First confirm with the real-Chrome smoke runner that `chrome.permissions.request` from a side-panel click shows Chrome's prompt.
   5. `/summarize` in the panel, with denylist checks and the summary write described in §5.2.
6. **M3–M5** follow as in the table. The compact visit-ID index, the Windows smoke run, and Windows directory-entry durability are release blockers tracked under M5.

## 13. Platforms and License

- **Browsers:** Google Chrome Stable only; see the release support window in §11. Edge and all other browsers are out of scope for the first release.
- **Operating systems:** macOS 15 Sequoia and later on Intel or Apple silicon; Windows 11 version 25H2 and later supported releases on x64. Windows 10, Windows on ARM, and Linux are out of scope for the first release.
- **License:** Apache-2.0, chosen for its explicit patent grant.

## 14. Open Questions

No open M0 or M1 design questions. The release hosting, support, platform, browser, and signing policies are defined in §11; M1 security requirements are recorded in §12.

M2 decisions (2026-09-28):

- **Extraction permissions.** Any page can be summarized. A side-panel click does not grant `activeTab`: Chrome's documentation lists only the action click, context menus, keyboard commands, and the omnibox, and Chromium withholds tab permissions from the side panel. The panel's summarize click therefore requests the page's exact origin when it is not already granted. Chromium's permission request needs only a user gesture and a browser window, so this is expected to show Chrome's prompt from the panel; confirm it with the smoke runner before building extraction (§12.2).
- **Summary destination.** A summary is a separate record from the page note: one Brauser-owned summary file per page, replaced whole on each new summary, linking to the page and its note (§5.2). The note holds only the user's own text. Because summaries never touch notes, M2 does not need the managed-block replacement in §6.4.
- **Harness modes.** Checked against the Claude Code 2.1.283 and codex-cli 0.144.4 documentation and help output (§7.1). Claude Code can run with no tools. Codex can run with its shell, web search, and user config disabled in a read-only sandbox, but documents no switch that removes every tool, so the host rejects any Codex run whose JSON event stream shows a tool call.
- **Denylist setup.** Required once: the first harness setup shows the agent denylist with its suggested categories, and no AI command runs until the user confirms it, even if they leave it empty (§7.3).

Side panel decisions (2026-09-28):

- **Editable notes.** A page note is a scratch space the user can add to at any time, edited only in the side panel and autosaved as they type (§5.3). This replaces M1's create-only notes and review drafts. The host replaces the note whole and refuses a save based on an old version (§4.4).
- **No AI output in notes.** Commands send output to the sidebar, a new separate file, or the clipboard (§5.6). The `page_note` output is removed.
- **Notes on any page.** Notes work on any HTTP(S) page; the first note on a site requests its origin from Chrome (§5.3).
- **`/log` on any page.** `/log` logs the current page now, even on a site not set up for logging, without enabling automatic logging there (§5.6).
- **No native confirmation for one-off actions.** Writing a note and running `/log` need only Chrome's site grant (§7). The trade-off: a compromised extension could write a note or log entry for any single page, where before it could do so only on sites the user confirmed. It still cannot enable automatic logging, change the notes folder, or weaken privacy settings without the host's confirmation.
