import assert from "node:assert/strict";
import test from "node:test";

// Run after build:extension. agent.js is pure: no chrome or DOM access.
const {
  CODEX_DISCLOSURE, SUGGESTED_EXCLUSIONS, addDenylistEntry, agentStateText, denylistEditsLive,
  harnessFoundText, harnessOfferNote, previewDenylistEntry, removeDenylistEntry,
} = await import("../dist/agent.js");
const { isConfig, isResponseShape } = await import("../dist/protocol-shape.js");

const config = {
  storage: {
    root: "/notes", profile: "neutral", log_dir: "log", pages_dir: "pages", later_dir: "later",
    summaries_dir: "summaries",
  },
  capture_enabled: false, sites: [], strip_params: [], near_repeat_secs: 300,
  agent_denylist: [], agent_denylist_confirmed: false, log_incognito: false, agent: null,
};

test("previewDenylistEntry_normalizes_case_and_idn", () => {
  assert.deepEqual(previewDenylistEntry("Bank.Example"), { ok: true, normalized: "bank.example" });
  assert.deepEqual(previewDenylistEntry("  mail.example  "), { ok: true, normalized: "mail.example" });
  assert.deepEqual(previewDenylistEntry("Bücher.example"), { ok: true, normalized: "xn--bcher-kva.example" });
  assert.deepEqual(previewDenylistEntry("192.168.0.1"), { ok: true, normalized: "192.168.0.1" });
  assert.deepEqual(previewDenylistEntry("[::1]"), { ok: true, normalized: "[::1]" });
  // The URL serialization, in hex, which is the form the host stores
  // (host/src/privacy.rs denylist_ipv6_uses_the_url_serialization).
  assert.deepEqual(previewDenylistEntry("[::FFFF:10.0.0.1]"), { ok: true, normalized: "[::ffff:a00:1]" });
  // The longest label and name the host accepts.
  const longest = `${Array(3).fill("a".repeat(63)).join(".")}.${"b".repeat(61)}`;
  assert.equal(longest.length, 253);
  assert.deepEqual(previewDenylistEntry(longest), { ok: true, normalized: longest });
});

test("previewDenylistEntry_rejects_scheme_path_port_wildcard_trailing_dot", () => {
  for (const raw of [
    "", "   ", "https://bank.example", "bank.example/login", "bank.example:443", "*.bank.example",
    "bank.example.", "bank example", "user@bank.example", "bank.example?x", "bank.example#x",
    "bank\\example", "[::1]:443", "mailto:bank.example",
    // The host's own rejects (host/src/privacy.rs): leading dot, empty
    // label, a label over 63 bytes, and a name over 253 bytes.
    ".a.com", "a..com", `${"a".repeat(64)}.com`, `${Array(5).fill("a".repeat(60)).join(".")}.com`,
  ]) {
    const preview = previewDenylistEntry(raw);
    assert.equal(preview.ok, false, raw);
    assert.equal(typeof preview.error, "string", raw);
  }
});

test("suggestions_are_valid_examples_and_editing_adds_only_the_typed_entry", () => {
  // The click handler that only fills the input is covered by the headless
  // smoke step "Suggestion only fills the box".
  assert.ok(SUGGESTED_EXCLUSIONS.length >= 4);
  assert.ok(Object.isFrozen(SUGGESTED_EXCLUSIONS));
  for (const suggestion of SUGGESTED_EXCLUSIONS) {
    assert.equal(typeof suggestion.label, "string");
    assert.equal(previewDenylistEntry(suggestion.example).ok, true, suggestion.example);
  }
  // Adding one entry adds exactly that entry, never a suggestion alongside it.
  const added = addDenylistEntry([], "Bank.Example");
  assert.deepEqual(added, { ok: true, list: ["bank.example"] });
  assert.deepEqual(addDenylistEntry(["bank.example"], "BANK.example").ok, false);
  const list = ["bank.example"];
  assert.deepEqual(removeDenylistEntry(list, "bank.example"), []);
  assert.deepEqual(list, ["bank.example"], "the input list is not changed");
  // The status text never claims an exclusion exists.
  const text = agentStateText({ state: "not_set_up", harness_version: null, message: null });
  for (const suggestion of SUGGESTED_EXCLUSIONS) assert.equal(text.includes(suggestion.example), false);
});

test("harness_found_text_asks_for_a_notes_folder_before_setup", () => {
  const withFolder = harnessFoundText("Claude Code", "2.1.284", true);
  assert.match(withFolder, /^Found Claude Code 2\.1\.284\./);
  assert.match(withFolder, /choose Set up harness/);
  const withoutFolder = harnessFoundText("Claude Code", "2.1.284", false);
  assert.match(withoutFolder, /^Found Claude Code 2\.1\.284\./);
  assert.match(withoutFolder, /Choose a notes folder first/);
  assert.doesNotMatch(withoutFolder, /then choose Set up harness\.$/);
  assert.match(harnessFoundText("Codex", null, true), /^Found Codex\. /);
});

test("denylist_edits_are_live_once_confirmed_even_without_harness", () => {
  assert.equal(denylistEditsLive(null), false);
  assert.equal(denylistEditsLive(config), false, "before setup, edits wait for its confirmation");
  const agent = {
    harness_id: "claude-code", adapter: "claude_code", binary: "/bin/claude", args: ["-p", "{prompt}"],
    env_allow: ["HOME", "PATH"], timeout_secs: 120,
  };
  assert.equal(denylistEditsLive({ ...config, agent_denylist_confirmed: true, agent }), true);
  // Removing the harness keeps the confirmation, so edits still save at once.
  assert.equal(denylistEditsLive({ ...config, agent_denylist_confirmed: true, agent: null }), true);
});

test("codex_offer_note_discloses_shared_codex_home", () => {
  const offer = {
    offer_id: "offer-1", adapter: "codex", harness_id: "codex", binary: "/bin/codex",
    real_path: "/bin/codex", version: "0.144.4", args: [], env_required: ["HOME", "PATH"],
    env_optional: [], refusal: null,
  };
  assert.match(CODEX_DISCLOSURE, /instructions \(AGENTS\.md\) and skills can shape/);
  assert.equal(harnessOfferNote(offer), `Ready to set up. ${CODEX_DISCLOSURE}`);
  assert.equal(harnessOfferNote({ ...offer, offer_id: null, refusal: "Codex 0.1.0 has not been reviewed" }),
    `Cannot be set up: Codex 0.1.0 has not been reviewed. ${CODEX_DISCLOSURE}`);
  const claude = { ...offer, adapter: "claude_code", harness_id: "claude-code" };
  assert.equal(harnessOfferNote(claude), "Ready to set up.");
});

test("agentStateText_describes_each_state", () => {
  assert.match(agentStateText(null), /unavailable/);
  assert.match(agentStateText({ state: "not_set_up", harness_version: null, message: "x" }), /off/);
  assert.match(agentStateText({ state: "denylist_unconfirmed", harness_version: null, message: null }),
    /privacy/);
  assert.equal(agentStateText({ state: "ready", harness_version: "2.1.284", message: null }),
    "AI harness ready (version 2.1.284).");
  assert.equal(
    agentStateText({ state: "harness_problem", harness_version: "2.1.284", message: "run setup again" }),
    "AI commands are off (version 2.1.284): run setup again");
});

test("isConfig_requires_v4_keys", () => {
  assert.equal(isConfig(config), true);
  for (const key of ["agent_denylist", "agent_denylist_confirmed", "log_incognito", "agent"]) {
    const without = { ...config };
    delete without[key];
    assert.equal(isConfig(without), false, key);
  }
  const { summaries_dir: _omitted, ...m1Storage } = config.storage;
  assert.equal(isConfig({ ...config, storage: m1Storage }), false);
});

test("isConfig_rejects_log_incognito_true", () => {
  assert.equal(isConfig({ ...config, log_incognito: true }), false);
  assert.equal(isConfig({ ...config, log_incognito: "false" }), false);
});

test("isResponseShape_accepts_new_responses_and_rejects_unknown", () => {
  const offer = {
    offer_id: "offer-1", adapter: "claude_code", harness_id: "claude-code",
    binary: "/bin/claude", real_path: "/opt/claude", version: "2.1.284", args: ["-p", "{prompt}"],
    env_required: ["HOME", "PATH"], env_optional: [{ name: "ANTHROPIC_API_KEY", present: false }],
    refusal: null,
  };
  assert.equal(isResponseShape({ type: "harnesses_discovered", offers: [offer] }), true);
  assert.equal(isResponseShape({
    type: "harness_setup_confirmed", harness_token: "t", config, summary: "s",
  }), true);
  assert.equal(isResponseShape({
    type: "agent_checked", harness_id: "claude-code", harness_version: "2.1.284", url_allowed: false,
  }), true);
  assert.equal(isResponseShape({ type: "harness_ready", offers: [offer] }), false);
  assert.equal(isResponseShape({ type: "agent_checked", harness_id: "claude-code" }), false);
});
