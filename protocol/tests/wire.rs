use brauser_protocol::{Request, Response};

// Every privacy and agent key is required on the wire, like `storage`.
const PRIVACY: &str =
    r#""agent_denylist":[],"agent_denylist_confirmed":false,"log_incognito":false,"agent":null"#;

fn config(storage: &str) -> String {
    format!(
        r#"{{{storage}"capture_enabled":false,"sites":[],"strip_params":["utm_*","fbclid","gclid"],"near_repeat_secs":300,{PRIVACY}}}"#
    )
}

fn update(config: &str) -> String {
    format!(
        r#"{{"type":"update_config","protocol_version":4,"request_id":"r1","expected_revision":"missing","picker_token":null,"consent_token":null,"harness_token":null,"config":{config}}}"#
    )
}

fn storage(summaries: Option<&str>) -> String {
    let summaries = summaries.map_or(String::new(), |value| {
        format!(r#","summaries_dir":{value}"#)
    });
    format!(
        r#""storage":{{"root":"/tmp/notes","profile":"neutral","log_dir":"log","pages_dir":"pages","later_dir":"later"{summaries}}},"#
    )
}

#[test]
fn omitted_storage_is_not_interpreted_as_explicit_null() {
    assert!(serde_json::from_str::<Request>(&update(&config(""))).is_err());
    assert!(serde_json::from_str::<Request>(&update(&config(r#""storage":null,"#))).is_ok());
}

#[test]
fn config_updates_require_a_revision_precondition() {
    let missing_revision = format!(
        r#"{{"type":"update_config","protocol_version":4,"request_id":"r1","picker_token":null,"consent_token":null,"harness_token":null,"config":{}}}"#,
        config(r#""storage":null,"#)
    );
    assert!(serde_json::from_str::<Request>(&missing_revision).is_err());
}

#[test]
fn config_responses_always_report_the_revision() {
    for kind in ["config_result", "config_updated"] {
        let extra = if kind == "config_result" {
            r#","config_issue":null,"agent_status":{"state":"not_set_up","harness_version":null,"message":null}"#
        } else {
            ""
        };
        let body = config(r#""storage":null,"#);
        let missing = format!(
            r#"{{"type":"{kind}","protocol_version":4,"request_id":"r1"{extra},"config":{body}}}"#
        );
        assert!(serde_json::from_str::<Response>(&missing).is_err());

        let present = format!(
            r#"{{"type":"{kind}","protocol_version":4,"request_id":"r1","revision":"missing"{extra},"config":{body}}}"#
        );
        assert!(serde_json::from_str::<Response>(&present).is_ok());
    }
}

#[test]
fn v4_config_requires_summaries_dir_key() {
    assert!(serde_json::from_str::<Request>(&update(&config(&storage(None)))).is_err());
    assert!(serde_json::from_str::<Request>(&update(&config(&storage(Some("null"))))).is_ok());
    assert!(
        serde_json::from_str::<Request>(&update(&config(&storage(Some(r#""summaries""#))))).is_ok()
    );
}

#[test]
fn v4_config_requires_privacy_keys() {
    for key in [
        r#""agent_denylist":[],"#,
        r#""agent_denylist_confirmed":false,"#,
        r#""log_incognito":false,"#,
        r#","agent":null"#,
    ] {
        let without = config(r#""storage":null,"#).replace(key, "");
        assert_ne!(
            without,
            config(r#""storage":null,"#),
            "{key} was not removed"
        );
        assert!(
            serde_json::from_str::<Request>(&update(&without)).is_err(),
            "config without {key} was accepted"
        );
    }
}

#[test]
fn update_config_requires_harness_token_key() {
    let without = update(&config(r#""storage":null,"#)).replace(r#""harness_token":null,"#, "");
    assert!(serde_json::from_str::<Request>(&without).is_err());
}

fn with_agent(agent: &str) -> String {
    update(&config(r#""storage":null,"#).replace(r#""agent":null"#, &format!(r#""agent":{agent}"#)))
}

#[test]
fn agent_config_rejects_unknown_fields() {
    let agent = r#"{"harness_id":"claude-code","adapter":"claude_code","binary":"/usr/local/bin/claude","args":["-p","{prompt}"],"env_allow":["HOME"],"timeout_secs":120}"#;
    assert!(serde_json::from_str::<Request>(&with_agent(agent)).is_ok());
    let extra = agent.replace(
        r#""timeout_secs":120"#,
        r#""timeout_secs":120,"stdin":"page""#,
    );
    assert!(serde_json::from_str::<Request>(&with_agent(&extra)).is_err());
}

#[test]
fn harness_adapter_rejects_hyphenated_value() {
    let agent = r#"{"harness_id":"claude-code","adapter":"claude-code","binary":"/usr/local/bin/claude","args":[],"env_allow":[],"timeout_secs":120}"#;
    assert!(serde_json::from_str::<Request>(&with_agent(agent)).is_err());
}

#[test]
fn check_agent_requires_url_key() {
    let with_url = r#"{"type":"check_agent","protocol_version":4,"request_id":"r1","url":null}"#;
    assert!(serde_json::from_str::<Request>(with_url).is_ok());
    let without = r#"{"type":"check_agent","protocol_version":4,"request_id":"r1"}"#;
    assert!(serde_json::from_str::<Request>(without).is_err());
}

#[test]
fn confirm_harness_setup_requires_every_key() {
    let full = r#"{"type":"confirm_harness_setup","protocol_version":4,"request_id":"r1","expected_revision":"missing","offer_id":"o1","env_names":["HOME"],"agent_denylist":[],"summaries_dir":null}"#;
    assert!(serde_json::from_str::<Request>(full).is_ok());
    for key in [
        r#","offer_id":"o1""#,
        r#","env_names":["HOME"]"#,
        r#","agent_denylist":[]"#,
        r#","summaries_dir":null"#,
    ] {
        assert!(
            serde_json::from_str::<Request>(&full.replace(key, "")).is_err(),
            "{key}"
        );
    }
    // The extension never names a binary; an extra path key is refused.
    let with_binary = full.replace(
        r#""offer_id":"o1""#,
        r#""offer_id":"o1","binary":"/bin/sh""#,
    );
    assert!(serde_json::from_str::<Request>(&with_binary).is_err());
}

#[test]
fn harnesses_discovered_offers_require_nullable_keys() {
    let offer = r#"{"offer_id":null,"adapter":"codex","harness_id":"codex","binary":"/usr/local/bin/codex","real_path":null,"version":null,"args":[],"env_required":["HOME","PATH"],"env_optional":[{"name":"OPENAI_API_KEY","present":false}],"refusal":"codex --version output was not recognized"}"#;
    let message = |offer: &str| {
        format!(
            r#"{{"type":"harnesses_discovered","protocol_version":4,"request_id":"r1","offers":[{offer}]}}"#
        )
    };
    assert!(serde_json::from_str::<Response>(&message(offer)).is_ok());
    for key in [
        r#""offer_id":null,"#,
        r#""real_path":null,"#,
        r#""version":null,"#,
    ] {
        assert!(
            serde_json::from_str::<Response>(&message(&offer.replace(key, ""))).is_err(),
            "{key}"
        );
    }
    let with_help = offer.replace(r#""args":[]"#, r#""args":[],"help":"usage""#);
    assert!(serde_json::from_str::<Response>(&message(&with_help)).is_err());
}

#[test]
fn harness_setup_confirmed_carries_token_config_and_summary() {
    let body = config(r#""storage":null,"#);
    let present = format!(
        r#"{{"type":"harness_setup_confirmed","protocol_version":4,"request_id":"r1","harness_token":"h1","config":{body},"summary":"Set up"}}"#
    );
    assert!(serde_json::from_str::<Response>(&present).is_ok());
    let without = present.replace(r#""harness_token":"h1","#, "");
    assert!(serde_json::from_str::<Response>(&without).is_err());
    let discover = r#"{"type":"discover_harnesses","protocol_version":4,"request_id":"r1","expected_revision":"missing"}"#;
    assert!(serde_json::from_str::<Request>(discover).is_ok());
}
