# Adapter Templates — Specification

> Status: Draft for review · Replaced the regular-expression patterns in DESIGN.md §7.2 (DESIGN.md updated 2026-09-28; §14 below is applied) · Target: M1.5a, built in DESIGN.md §12.2 step 6.2 · Format version 1

## 1. Purpose

An artifact adapter (DESIGN.md §5.1, §7.2) says "this URL is Jira issue `PROJ-123` in `acme.atlassian.net`." Until now, adapters did this with regular expressions, which caused three problems:

1. The worker must run the same patterns as the host, and JavaScript's backtracking `RegExp` can disagree with Rust's `regex` and can run in polynomial time on hostile input. That needed a restricted dialect, a host-side rewrite, and a two-engine conformance corpus.
2. Nobody can review a regular expression by reading it, whether a user wrote it or an agent drafted it (Tier 2).
3. Nothing stops a pattern from matching every page on an origin, so Tier 2 needed a list of rejection rules for drafts.

Adapter templates replace regular expressions with a small declarative language:

- **Path templates** match a URL's path one segment at a time: `/browse/{key:jira_key}`.
- **Query conditions** match named query parameters: `selectedIssue = "{key:jira_key}"`.
- **Title templates** extract a title and fields from the tab title: `[{key:jira_key}] {title} - Jira`.
- **ID templates** build the canonical `artifact_id`: `jira:{host}/{key}`.

Captures are typed with a fixed set of host-owned types. Every operation runs in time linear in its input with no backtracking, so the host (Rust) and the worker (TypeScript) implement the same small grammar and cannot diverge in any way that matters. Raw regular expressions are not supported in format version 1.

**Terminology.** "Template" now means this syntax. The adapters Brauser ships for common tools, previously called templates, are called **starter adapters**.

## 2. Goals and non-goals

Goals:

- Every starter adapter in DESIGN.md §7.2 can be expressed.
- Matching is linear in input length, in both engines, by construction.
- Two independent implementations produce identical results on every input, verified in CI.
- A template can be reviewed by reading it, and the host can decide mechanically whether two templates overlap.
- Captured values can never produce a path, an ambiguous ID, or a control character.

Non-goals for format version 1:

- Matching URL fragments. Hash-routed apps (`/#/issue/123`) are out of scope (§13).
- User-defined capture types.
- Matching page content. Tier 1 fields remain CSS selectors (DESIGN.md §5.1).

## 3. Adapter structure

```toml
[adapters]
format = 1                     # this specification; required

[[artifacts]]
type     = "jira.issue"        # adapter type, [a-z][a-z0-9._]{0,31}
origin   = "https://acme.atlassian.net"
match    = [                   # one or more rules, tried in order
  { path = "/browse/{key:jira_key}" },
  { path = "/jira/{*}", query = { selectedIssue = "{key:jira_key}" } },
]
id       = "jira:{host}/{key}"
title    = "[{key:jira_key}] {title} - Jira"      # optional
alias    = { from = "tab_title", capture = "key" } # optional (§9)
refs     = [{ type = "jira.issue", find = "jira_key" }] # optional (§8)
modes    = { }                 # optional (§5.4)
scope    = { }                 # required on shared origins (§6)
path_case = "sensitive"        # or "insensitive" (§5.1)
fields   = { status = "<selector>" }               # Tier 1, unchanged
```

Each rule is a table with:

| Key | Required | Meaning |
|---|---|---|
| `path` | yes | A path template (§4) |
| `query` | no | Query conditions (§5.3), at most 8 |
| `absent` | no | Query keys that must not be present, at most 8 |
| `mode` | no | A fixed mode for this rule: `view`, `edit`, or `review` |

The `format` key is required whenever `[[artifacts]]` entries exist. A `format` the host does not support makes the config newer than the host (DESIGN.md §3.2): the extension policy is suspended, nothing is deleted, and settings offers repair.

A rule matches when its path template, every query condition, and every `absent` key match. The first matching rule of an adapter wins, and adapters on one origin are tried in config order. §10.3 describes how the host reports adapters that can match the same URL.

## 4. Path templates

### 4.1 Grammar

```ebnf
path_template = "/" , [ segment , { "/" , segment } ] ;
segment       = literal | alternation | capture | optional | anonymous | rest ;

literal       = lit_char , { lit_char } ;
lit_char      = ASCII printable except  / { } ( ) | ? # % space ;
alternation   = "(" , literal , { "|" , literal } , ")" ;

capture       = "{" , name , ":" , ctype , "}" ;
optional      = "{" , name , "?:" , ctype , "}" ;
anonymous     = "{_}" ;
rest          = "{*}" ;

name          = lower , { lower | digit | "_" } ;          (* at most 32 *)
ctype         = alternation
              | type_name , [ bounds ] , [ case_mod ] , [ "@tail" ] ;
type_name     = "int" | "hex" | "token" | "name" | "jira_key" | "uuid" ;
bounds        = "(" , int , [ "," , int ] , ")" ;           (* narrows only *)
case_mod      = "|lower" | "|upper" ;
```

Structural rules, checked when config loads:

- `{*}` may appear only as the last segment.
- Optional segments may appear only after every required segment, and only before `{*}` if one is present.
- A template must contain at least one literal or alternation segment. `/` alone and `/{*}` are invalid.
- Capture names are unique within a rule.
- At most 32 segments, 16 alternatives per alternation, 64 characters per literal, and 512 characters per template.
- `bounds` may only narrow a type's default length range (§7). `hex(32)` means exactly 32; `token(20,64)` means 20 to 64.
- `@tail` is described in §7.2.
- In a capture of an alternation (`{mode:(edit|view)}`), every literal uses only the ID-literal characters `a-z`, `0-9`, `.`, `_`, and `-` (§8.1), and none is `.` or `..`. The literal becomes the captured value and may appear in an ID, so it must not contain `:` or uppercase letters. Plain alternations that bind nothing, such as `(file|design)`, may use any `lit_char`.

### 4.2 Segment kinds

| Kind | Example | Matches one decoded segment that… | Binds |
|---|---|---|---|
| Literal | `browse` | equals the literal (case per `path_case`) | nothing |
| Alternation | `(file|design)` | equals one of the literals | nothing |
| Capture | `{key:jira_key}` | is a valid value of the type | canonical value |
| Capture of alternation | `{mode:(edit|view)}` | equals one of the literals | the literal |
| Optional | `{mode?:(edit|view)}` | as the capture, or is absent (§4.3) | value, or unbound |
| Anonymous | `{_}` | is any non-empty segment of at most 256 characters | nothing |
| Rest | `{*}` | zero or more remaining segments | nothing |

### 4.3 Path matching

The algorithm is normative. Let the template have required segments `R` (literals, alternations, captures, and anonymous segments), then optional segments `O`, then an optional rest. Let the URL's decoded segments be `S` (§5.1), with `n = |S|`.

1. If `n < |R|`, the rule does not match.
2. If there is no rest and `n > |R| + |O|`, the rule does not match.
3. For `i` in `0 .. |R|`: `R[i]` must match `S[i]`, or the rule does not match.
4. For `j` in `0 .. |O|`, while `|R| + j < n`: if `O[j]` matches `S[|R| + j]`, bind it and continue. If it does not match:
   - with a rest, `O[j]` and every later optional are unbound, and the remaining segments go to the rest;
   - without a rest, the rule does not match.
5. Any segments left over go to the rest. Without a rest, step 2 guarantees there are none.

Every step examines each segment at most once, and no earlier decision is revisited. Matching one rule is `O(total length of S)`.

## 5. URL handling

### 5.1 From URL to segments

Both engines prepare the URL identically before any rule is tried:

1. The URL is capped at 2048 characters; a longer URL matches no adapter.
2. The URL is parsed with WHATWG URL semantics. Its scheme, host, and port must equal the adapter's `origin` exactly; otherwise the adapter does not apply.
3. The raw path (before percent-decoding) is split on `/`. The leading empty element is dropped. A single trailing empty element (a trailing slash) is dropped. Any other empty segment (`//`) means the URL matches no rule.
4. Each segment is percent-decoded once as UTF-8. Invalid percent-encoding or invalid UTF-8 means no match.
5. A decoded segment that contains `/` (from `%2F`), a control character (U+0000–U+001F or U+007F), or that is exactly `.` or `..`, means the URL matches no rule.
6. At most 64 segments; more means no match.

With `path_case = "insensitive"`, literals and alternations compare with ASCII case folding. Capture types define their own case handling (§7). The default is `sensitive`.

### 5.2 Why decode, and why reject `%2F`

Literals and types compare against decoded text, so `Doc.aspx` and `Doc%2Easpx` behave the same, and a UUID in `sourcedoc=%7B…%7D` is read with its braces. An encoded slash inside a segment would make "one segment" mean two different things in two places. None of the starter adapters' tools needs one, so format version 1 refuses such URLs rather than defining an interpretation.

### 5.3 Query conditions

The query string is parsed with WHATWG `URLSearchParams` semantics, including `+` as a space. Keys compare exactly and case-sensitively. A condition's value is one of:

| Form | Example | Matches when the key is present once and its value… |
|---|---|---|
| Capture | `"{key:jira_key}"` | is a valid value of the type, as a whole |
| Literal | `"edit"` | equals the literal |
| Presence | `"{_}"` | is non-empty |

A key a condition references that appears more than once means the rule does not match, because the value would be ambiguous. Keys no condition references are ignored. `absent = ["q", "jql"]` requires each listed key to be missing.

### 5.4 Modes

A mode is `view`, `edit`, or `review` (DESIGN.md §5.1). A rule gets its mode from, in order:

1. A capture named `mode` whose type is an alternation, mapped through the adapter's `modes` table, for example `modes = { edit = "edit", preview = "view" }`. Every literal in that alternation must appear in `modes`.
2. The rule's fixed `mode` key.
3. Otherwise, no mode is recorded.

A capture named `mode` is never used in an ID. A capture named `mode` whose type is not an alternation is a validation error.

## 6. Scope on shared origins

Some origins host many unrelated tenants: every GitHub organization, every Google account in a Chrome profile, every Notion workspace. An adapter recognizes artifacts by URL shape, and shape alone cannot tell work from personal. Scope closes that gap.

The host owns a list of **shared origins**. Format version 1 lists `https://github.com`, `https://docs.google.com`, `https://www.notion.so`, `https://www.figma.com`, and `https://linear.app`. The list is host-owned config and can grow in later releases.

An adapter on a shared origin must have one of:

- **`scope`**, which restricts one or more captures to a list of literal values:

  ```toml
  scope = { owner = ["acme-corp", "acme-labs"] }
  ```

  Each scoped capture must be bound by every rule of the adapter, and its canonical value (§7) must equal one of the listed values. Each listed value must itself be a valid canonical value of that capture's type, bounds, and case modifier; a value no URL could produce, such as `"Acme-Corp"` for a `name|lower` capture, is a validation error rather than a scope that silently matches nothing. A URL that fails scope is not an artifact, and the worker drops it like any unrecognized URL.

- **`unscoped = "acknowledged"`**, for origins where the URL cannot tell tenants apart. The native confirmation says so in plain words, for example: "This records every Google Doc you open in this Chrome profile, from every Google account signed in to it." It recommends a separate Chrome profile for work. Google's `/u/<n>/` account index is not accepted as a scope, because the index depends on sign-in order.

Widening a scope (adding a value, or switching to `unscoped`) widens capture and needs the native confirmation. Narrowing takes effect immediately.

## 7. Capture types

### 7.1 Types

All types are fixed by the host. Every type's alphabet is ASCII, and no type admits `/`, `:`, `{`, `}`, `%`, whitespace, or control characters. That is what makes IDs parse unambiguously and keeps captured values out of paths.

| Type | Accepts | Default length | Canonical value |
|---|---|---|---|
| `int` | ASCII digits, without a leading zero unless the value is `0` | 1–19 | as matched |
| `hex` | `0-9`, `a-f`, `A-F` | 1–64 | lowercase |
| `token` | `A-Z`, `a-z`, `0-9`, `_`, `-` | 1–128 | as matched |
| `name` | `A-Z`, `a-z`, `0-9`, `.`, `_`, `-`, but not `.` or `..` | 1–100 | as matched |
| `jira_key` | a letter, then letters, digits, or `_`; then `-`; then an `int`. Letters match in either case | project part 1–255 | uppercase |
| `uuid` | 8-4-4-4-12 hex digits, optionally wrapped in `{ }` | fixed | lowercase, braces removed |
| alternation | one of the listed literals (ID-literal characters only when captured, §4.1) | — | the literal |

`|lower` and `|upper` apply ASCII case mapping to the canonical value. They are allowed only on `token` and `name`, the two types that preserve case by default. Use them when the tool treats the value case-insensitively (GitHub owners and repositories) and never when it does not (Google document IDs are case-sensitive).

Length is counted in characters of the decoded segment or query value. A value outside its length range does not match.

### 7.2 `@tail`

Some tools put a title slug and an ID in one segment, such as Notion's `/acme/Release-Plan-0123456789abcdef0123456789abcdef`. `{id:hex(32)@tail}` matches a segment when the text after its last `-` matches the type, or the whole segment matches when it has no `-`. The rest of the segment is ignored. `@tail` is allowed only on `int` and `hex`. The other types (`token`, `name`, `jira_key`, and `uuid`) include `-` in their alphabet, so the text after the last `-` would be only part of a value, and they could not be split unambiguously.

## 8. ID templates

### 8.1 Grammar

```ebnf
id_template = scheme , ":" , "{host}" , { "/" , id_part } ;
scheme      = lower , { lower | digit | "." } ;       (* at most 32 *)
id_part     = "{" , name , "}" | id_literal ;
id_literal  = ( lower | digit | "." | "_" | "-" ) , { lower | digit | "." | "_" | "-" } ;
```

- `{host}` is the adapter origin's host, lowercased, with the port appended as `:port` only when it is not the scheme's default.
- Every `{name}` must be a capture that **every** rule of the adapter binds, and never an optional capture. Every rule must bind exactly the captures the ID uses, plus optionally `mode` and scoped captures; any other named capture is a validation error. Use `{_}` for segments that are matched but not used.
- A built ID is at most 512 bytes.

Because no capture type admits `/` or `:`, an `artifact_id` splits back into its parts unambiguously.

### 8.2 Stability

`artifact_id` is the record key for notes, summaries, read-later entries, artifact files, and edges (DESIGN.md §6.2), so an adapter's ID template is a durable contract:

- The `format` number versions this language. A future format may add types or segment kinds, but it never changes how an existing template in an older format builds an ID.
- Changing the `id` template of an adapter that has recorded activity is a **migration**, not an edit. The host shows how many artifacts are affected, and on confirmation writes one `alias` event from each old ID to its new one (DESIGN.md §5.1), so no history, note, or summary is orphaned.
- A starter adapter's ID template is frozen once it ships. The ID scheme alignment question in DESIGN.md §14 must be settled before the first starter adapters ship.

## 9. Title templates

### 9.1 Grammar and semantics

```ebnf
title_template = { t_literal | t_capture | skip } , "{title}" , { t_literal | t_capture | skip } ;
t_literal      = one or more Unicode scalar values except "{" and "}" ;
t_capture      = "{" , name , ":" , type_name , [ bounds ] , "}" ;
skip           = "{~}" ;
```

A title template has exactly one `{title}` placeholder, which splits it into a prefix `P` and a suffix `Q`. Matching is anchored at both ends of the tab title:

1. `P` is matched left to right from the start of the tab title. A literal must match exactly. A typed capture takes the longest run of its type's alphabet, and the run must then be a valid value. `{~}` must be followed by a literal and skips forward to the nearest occurrence of that literal.
2. `Q` is matched right to left from the end, in mirror image: typed captures take the longest run leftward, and `{~}` must be preceded by a literal and skips back to the nearest occurrence of that literal.
3. What lies between the end of `P` and the start of `Q` is the title, trimmed of surrounding whitespace. If `P` and `Q` overlap, or the title is empty, the template does not match.

Validation requires a literal between any two captures or skips, and between a capture or skip and `{title}`, so every capture is bounded by a literal. The literal that bounds a typed capture must not be swallowed by it: in `P`, the literal after a capture must start with a character outside the capture type's alphabet, and in `Q`, the literal before a capture must end with such a character. For example, `{key:jira_key}-…` is invalid, because `-` is in the `jira_key` alphabet.

If the template does not match, Brauser records the whole tab title, as today. A typed capture with the same name as a path or query capture must use the same type, and it is canonicalized with that capture's bounds and case modifier (so a title `{owner:name}` is lowercased when the path has `{owner:name|lower}`). The two values must then agree; if they do not, the template counts as not matching, except when an `alias` rule names that capture (§9.3). Title captures with any other name are validation errors: use `{~}` to skip text you do not need.

Literals compare by Unicode scalar value with no normalization, so GitHub's `·` (U+00B7) is written as itself. Before matching, the worker converts the title to well-formed Unicode (`String.prototype.toWellFormed()`, replacing lone surrogates with U+FFFD) and caps it at 512 scalar values. The host applies the same steps. Both engines count and compare in scalar values, never in UTF-16 code units.

### 9.2 Why this is enough

Matching `P` forward and `Q` backward, with skips that stop at the nearest delimiter, handles titles that contain the delimiters themselves. For the Confluence title `A - B - Team Space - Confluence`, the template `{title} - {~} - Confluence` matches ` - Confluence` from the end, skips back to the nearest ` - `, and leaves `A - B` as the title. Every step scans each character at most once.

### 9.3 Aliases

```toml
alias = { from = "tab_title", capture = "key" }
alias = { from = "meta", name = "ajs-page-id", value = "{id:int}" }
```

A `tab_title` alias names a title capture. A `meta` alias names one `<meta>` element and a value template that is a single capture matching the element's whole `content`. Either one yields a second ID through the adapter's `id` template.

Both signals come from text the page controls. They are recorded as **suggested** aliases, listed on the artifact card but not grouped, and change record keys or group activity only after the user confirms them on the artifact card or a server redirect between the same two IDs corroborates them (DESIGN.md §5.1, §6.2). Server-redirect aliases need no declaration.

## 10. Refs, evaluation, and validation

### 10.1 Refs

```toml
refs = [{ type = "jira.issue", find = "jira_key" }]
```

A ref scans the extracted title (the `{title}` part only, so an artifact never refers to itself through its own title prefix) for values of the named type. A value counts only when the character before and after it is not an ASCII letter, digit, or `_`. In refs, `jira_key` matches only as written in uppercase (an uppercase letter, then uppercase letters, digits, or `_`), so ordinary words such as `covid-19` or `python-3` never match. Uppercase technical terms such as `UTF-8` or `SHA-256` still have the shape of a key, so a resolved ref becomes a *mentions* edge only once its target ID has been recorded as an artifact; until then it is kept as an unresolved key and not shown. Scanning is bounded by the title's length times the type's maximum length. At most 20 distinct refs are kept per event. A ref resolves only when exactly one enabled adapter has the target type and its ID template has exactly one placeholder besides `{host}`; otherwise it stays an unresolved key, as today.

### 10.2 Evaluation guarantee

For one URL and title, evaluation is linear in the input size (capped at 2048 and 512 characters) times the number of rules. No step backtracks. Both engines implement §4.3, §5, §7, and §9 directly; neither compiles templates to regular expressions.

The host remains authoritative. The worker's copy only prefilters, so a worker bug or disagreement can only drop a visit the host would have recorded, never record one the host would reject (DESIGN.md §3.2).

### 10.3 Validation and overlap

When config loads, the host checks every rule in §4.1, §5, §6, §7, §8, and §9 and reports the first error with the adapter, rule, and character position. Limits: at most 64 adapters, 16 rules per adapter.

Because the language has no free-form patterns, the host can decide whether two rules could match the same URL. It compares them segment by segment: literals against literals, literals against type membership, types against types by alphabet and length range, with optional segments and rests expressed as length ranges. The comparison is conservative: it treats `@tail`, `jira_key`'s structure, and query conditions by their alphabets and length ranges, so it can report an overlap that no URL actually produces, but it never misses one. Messages therefore say "may overlap". The host uses this to:

- warn when two user-written adapters on one origin overlap, naming the rule that would win;
- reject a Tier 2 draft that overlaps an existing adapter on its origin;
- list, in the match preview, which already-recognized artifacts a change would re-map.

### 10.4 Tier 2 drafts

A drafted adapter is written in this language, and the structural rules replace most of the regex-inspection rules in DESIGN.md §5.1:

- A draft may not use `title`, `scope`, `unscoped`, `alias`, `fields`, or `path_case`. The agent never sees a title (DESIGN.md §5.1), so a drafted title template would be a guess. On a shared origin, the host copies the scope of the origin's existing scoped adapter into the draft. If the origin has several scoped adapters and their scopes differ, or none, the draft is rejected, and the user can write the adapter by hand.
- Every rule must have at least one literal segment before its first capture, and its ID must use at least one capture whose type is not an alternation.
- `{*}` may not directly follow the first segment. (A rule of literals, `{*}`, and `absent` conditions alone already fails validation, because every rule must bind the ID's captures, §8.1.)
- The host still rejects drafts that match the origin root or any probe URL, and still shows the match preview before the native confirmation.

The instruction to the agent includes this specification's grammar, and the host parses the draft with the same parser as any config. A draft that does not parse is discarded without being shown.

## 11. Starter adapters

These examples show the language on each shape it has to handle. URL and title formats must be checked against each live tool on the date each starter adapter records (DESIGN.md §7.2); treat the ones below as illustrative until then.

```toml
[adapters]
format = 1

# Jira Cloud: issue page and board modal; title carries the key.
[[artifacts]]
type   = "jira.issue"
origin = "https://<your-site>.atlassian.net"
match  = [
  { path = "/browse/{key:jira_key}" },
  { path = "/jira/{*}", query = { selectedIssue = "{key:jira_key}" } },
]
id     = "jira:{host}/{key}"
title  = "[{key:jira_key}] {title} - Jira"
alias  = { from = "tab_title", capture = "key" }
refs   = [{ type = "jira.issue", find = "jira_key" }]

# Confluence Cloud: title slug after the ID; edit path; space name skipped in the title.
[[artifacts]]
type   = "confluence.page"
origin = "https://<your-site>.atlassian.net"
match  = [
  { path = "/wiki/spaces/{_}/pages/{id:int}/{*}" },
  { path = "/wiki/spaces/{_}/pages/edit-v2/{id:int}", mode = "edit" },
]
id     = "confluence:{host}/{id}"
title  = "{title} - {~} - Confluence"

# Google Docs: shared origin with no tenant in the URL; optional mode. No rest, so /copy and /export are not opens.
[[artifacts]]
type     = "gdoc"
origin   = "https://docs.google.com"
unscoped = "acknowledged"
match    = [
  { path = "/document/d/{id:token(20,64)}/{mode?:(edit|view|preview)}" },
  { path = "/document/u/{_}/d/{id:token(20,64)}/{mode?:(edit|view|preview)}" },
]
modes    = { edit = "edit", view = "view", preview = "view" }
id       = "gdoc:{host}/{id}"
title    = "{title} - Google Docs"

# GitHub pull requests: scoped to an org; case-folded names; review tab.
[[artifacts]]
type   = "github.pr"
origin = "https://github.com"
match  = [
  { path = "/{owner:name|lower}/{repo:name|lower}/pull/{number:int}/{mode?:(files|commits|checks)}/{*}" },
]
scope  = { owner = ["acme-corp"] }
modes  = { files = "review", commits = "view", checks = "view" }
id     = "github.pr:{host}/{owner}/{repo}/{number}"
title  = "{title} by {~} · Pull Request #{number:int} · {owner:name}/{repo:name} · GitHub"
refs   = [{ type = "jira.issue", find = "jira_key" }]

# Notion: ID at the end of a slugged segment; scoped to a workspace.
[[artifacts]]
type   = "notion.page"
origin = "https://www.notion.so"
match  = [{ path = "/{workspace:name|lower}/{id:hex(32)@tail}" }]
scope  = { workspace = ["acme"] }
id     = "notion:{host}/{id}"

# SharePoint: identity in a braced GUID in the query; case-insensitive paths.
[[artifacts]]
type      = "sharepoint.doc"
origin    = "https://<tenant>.sharepoint.com"
path_case = "insensitive"
match     = [
  { path = "/sites/{_}/_layouts/15/Doc.aspx",       query = { sourcedoc = "{doc:uuid}" } },
  { path = "/:w:/r/sites/{_}/_layouts/15/Doc.aspx", query = { sourcedoc = "{doc:uuid}" } },
  { path = "/personal/{_}/_layouts/15/Doc.aspx",    query = { sourcedoc = "{doc:uuid}" } },
]
id        = "sharepoint:{host}/{doc}"
```

Known limits these examples show: Notion URLs with no workspace segment do not match a scoped Notion adapter; the Jira board-modal rule matches any Jira page carrying `selectedIssue`; and Google Docs cannot be scoped by URL.

## 12. Implementation and testing

- **Two implementations.** A Rust crate in the host workspace and a TypeScript module in the extension, each implementing the parser, validation for what the worker needs, and evaluation. Neither uses a regular-expression engine. Each is expected to be a few hundred lines.
- **Shared test vectors.** `/protocol/adapter-templates/vectors.json` holds templates, inputs, and expected results (match, captures, canonical ID, title, mode, or the specific error). Both implementations run every vector in CI.
- **Differential property tests.** A generator produces random valid templates and URLs and titles derived from them, including near misses (one character changed, a segment added or removed, encoded characters). A host CLI (`brauser adapters eval`) and a Node harness evaluate each case, and any difference fails CI.
- **Fuzzing.** `cargo-fuzz` targets the template parser, URL preparation (§5.1), and title matching.
- **Bounds tests.** Inputs at every cap (2048-character URL, 64 segments, 512-character title, longest type values) complete within a fixed time budget in both engines.
- **Scope tests.** For each shared origin: a URL outside the scope, a case variant of a scoped value, and a scoped capture missing from one rule (a validation error).

## 13. Open questions

- **Hash-routed apps.** Fragments are ignored. Supporting `/#/…` routes would mean matching the fragment as a second path, which is easy to add in a later format if a real tool needs it.
- **Notion without a workspace segment.** Should a scoped Notion adapter fall back to Tier 1 (reading the workspace from the page), or should those pages simply go unrecorded?
- **Unread-count prefixes.** Some tools prefix titles with `(3) `. Should title preparation strip a leading `(<int>) ` for every adapter, or should each title template handle it?
- **Shared-origin list.** Which further origins belong on it at release (for example Atlassian's `id.atlassian.com`, or `gitlab.com`)?

## 14. Changes to DESIGN.md

- **§7.2** — Replace the regex example, "Evaluation", and "The worker's copy" with a pointer to this specification. Rename "Templates" to "Starter adapters". Add `scope`, `unscoped`, and `path_case` to the adapter description, and add the shared-origin rule to "Consent".
- **§5.1 Tier 2** — Replace the regex rejection list with §10.4. Keep the probe URLs and the match preview.
- **§5.1 Aliases** — Title and meta aliases become suggested until confirmed or corroborated (§9.3).
- **§4.1** — Replace the "Hostile adapter patterns" row: templates are linear by construction in both engines, captures cannot form paths or ambiguous IDs, and scope limits shared origins. Add a row for personal activity on shared origins, mitigated by scope and the unscoped acknowledgment.
- **§6.2** — Add that an `id` template change is a migration (§8.2).
- **§12.2 step 6.2** (now split into step 6.2, the engine, and step 6.3, config and lease) — Replace the regex tests (pathological pattern, nested quantifier, two-engine conformance corpus) with §12 of this specification.
- **§14** — Record the decision: adapter templates replace regular expressions in format version 1, with no raw-regex escape hatch.
