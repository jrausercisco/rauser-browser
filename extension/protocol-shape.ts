/** Runtime shape checks for host responses. The generated types describe the
 * protocol, but a native host is a separate process and its JSON is checked
 * before the extension trusts it. */

const ERROR_CODES: ReadonlySet<string> = new Set([
  "invalid_request", "unsupported_protocol_version", "invalid_config", "not_configured",
  "unauthorized", "conflict", "message_too_large", "internal", "cancelled",
]);

const AGENT_STATES: ReadonlySet<string> = new Set([
  "not_set_up", "denylist_unconfirmed", "harness_problem", "ready",
]);

function stringOrNull(value: unknown): boolean {
  return value === null || typeof value === "string";
}

function isStringArray(value: unknown): boolean {
  return Array.isArray(value) && value.every((item: unknown) => typeof item === "string");
}

function isAgent(value: unknown): boolean {
  if (value === null) return true;
  if (typeof value !== "object") return false;
  const agent = value as Record<string, unknown>;
  return typeof agent.harness_id === "string" &&
    (agent.adapter === "claude_code" || agent.adapter === "codex") &&
    typeof agent.binary === "string" &&
    isStringArray(agent.args) && isStringArray(agent.env_allow) &&
    typeof agent.timeout_secs === "number" && Number.isInteger(agent.timeout_secs);
}

function isEnvName(value: unknown): boolean {
  if (typeof value !== "object" || value === null) return false;
  const env = value as Record<string, unknown>;
  return typeof env.name === "string" && typeof env.present === "boolean";
}

function isHarnessOffer(value: unknown): boolean {
  if (typeof value !== "object" || value === null) return false;
  const offer = value as Record<string, unknown>;
  return stringOrNull(offer.offer_id) &&
    (offer.adapter === "claude_code" || offer.adapter === "codex") &&
    typeof offer.harness_id === "string" && typeof offer.binary === "string" &&
    stringOrNull(offer.real_path) && stringOrNull(offer.version) &&
    isStringArray(offer.args) && isStringArray(offer.env_required) &&
    Array.isArray(offer.env_optional) && offer.env_optional.every(isEnvName) &&
    stringOrNull(offer.refusal);
}

function isAgentStatus(value: unknown): boolean {
  if (typeof value !== "object" || value === null) return false;
  const status = value as Record<string, unknown>;
  return typeof status.state === "string" && AGENT_STATES.has(status.state) &&
    stringOrNull(status.harness_version) && stringOrNull(status.message);
}

export function isConfig(value: unknown): boolean {
  if (typeof value !== "object" || value === null) return false;
  const config = value as Record<string, unknown>;
  const storage = config.storage;
  if (storage !== null) {
    if (typeof storage !== "object" || storage === null) return false;
    const record = storage as Record<string, unknown>;
    if (!["root", "profile", "log_dir", "pages_dir", "later_dir"]
      .every((key) => typeof record[key] === "string")) return false;
    if (!stringOrNull(record.summaries_dir)) return false;
  }
  return typeof config.capture_enabled === "boolean" &&
    Array.isArray(config.sites) &&
    config.sites.every((site: unknown) =>
      typeof site === "object" && site !== null &&
      typeof (site as Record<string, unknown>).origin === "string" &&
      typeof (site as Record<string, unknown>).path_prefix === "string") &&
    isStringArray(config.strip_params) &&
    typeof config.near_repeat_secs === "number" &&
    Number.isInteger(config.near_repeat_secs) &&
    isStringArray(config.agent_denylist) &&
    typeof config.agent_denylist_confirmed === "boolean" &&
    config.log_incognito === false &&
    isAgent(config.agent);
}

export function isResponseShape(value: Record<string, unknown>): boolean {
  switch (value.type) {
    case "error":
      return typeof value.code === "string" && ERROR_CODES.has(value.code) &&
        typeof value.message === "string";
    case "hello_result":
      return typeof value.host_version === "string" &&
        typeof value.configured === "boolean" && stringOrNull(value.config_issue);
    case "config_result":
      return typeof value.revision === "string" && isConfig(value.config) &&
        stringOrNull(value.config_issue) && isAgentStatus(value.agent_status);
    case "config_updated":
      return typeof value.revision === "string" && isConfig(value.config);
    case "folder_chosen":
      return typeof value.path === "string" && typeof value.picker_token === "string";
    case "config_confirmed":
      return typeof value.consent_token === "string" && typeof value.summary === "string";
    case "visit_recorded":
      return typeof value.event_id === "string" &&
        ["persisted", "suppressed", "rejected", "retryable"].includes(String(value.outcome)) &&
        stringOrNull(value.reason) && stringOrNull(value.relative_path);
    case "note_loaded":
      return typeof value.exists === "boolean" && typeof value.revision === "string" &&
        typeof value.title === "string" && typeof value.body === "string";
    case "note_saved":
      return ["created", "replaced", "created_with_warning", "replaced_with_warning"]
        .includes(String(value.outcome)) &&
        typeof value.revision === "string" && typeof value.relative_path === "string";
    case "note_conflict":
      return typeof value.exists === "boolean" && typeof value.revision === "string" &&
        typeof value.title === "string" && typeof value.body === "string";
    case "agent_checked":
      return typeof value.harness_id === "string" && typeof value.harness_version === "string" &&
        (value.url_allowed === null || typeof value.url_allowed === "boolean");
    case "harnesses_discovered":
      return Array.isArray(value.offers) && value.offers.every(isHarnessOffer);
    case "harness_setup_confirmed":
      return typeof value.harness_token === "string" && isConfig(value.config) &&
        typeof value.summary === "string";
    default:
      return false;
  }
}
