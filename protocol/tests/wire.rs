use rauser_protocol::{Request, Response};

#[test]
fn omitted_storage_is_not_interpreted_as_explicit_null() {
    let missing = r#"{
        "type":"update_config",
        "protocol_version":2,
        "request_id":"r1",
        "expected_revision":"missing","picker_token":null,"consent_token":null,
        "config":{"capture_enabled":false,"sites":[],"strip_params":["utm_*","fbclid","gclid"],"near_repeat_secs":300}
    }"#;
    assert!(serde_json::from_str::<Request>(missing).is_err());

    let explicit_null = r#"{
        "type":"update_config",
        "protocol_version":2,
        "request_id":"r1",
        "expected_revision":"missing","picker_token":null,"consent_token":null,
        "config":{"storage":null,"capture_enabled":false,"sites":[],"strip_params":["utm_*","fbclid","gclid"],"near_repeat_secs":300}
    }"#;
    assert!(serde_json::from_str::<Request>(explicit_null).is_ok());
}

#[test]
fn config_updates_require_a_revision_precondition() {
    let missing_revision = r#"{
        "type":"update_config",
        "protocol_version":2,
        "request_id":"r1",
        "picker_token":null,
        "consent_token":null,
        "config":{"storage":null,"capture_enabled":false,"sites":[],"strip_params":["utm_*","fbclid","gclid"],"near_repeat_secs":300}
    }"#;
    assert!(serde_json::from_str::<Request>(missing_revision).is_err());
}

#[test]
fn config_responses_always_report_the_revision() {
    for kind in ["config_result", "config_updated"] {
        let repair_field = if kind == "config_result" {
            ",\"config_issue\":null"
        } else {
            ""
        };
        let missing = format!(
            r#"{{"type":"{kind}","protocol_version":2,"request_id":"r1"{repair_field},"config":{{"storage":null,"capture_enabled":false,"sites":[],"strip_params":["utm_*","fbclid","gclid"],"near_repeat_secs":300}}}}"#
        );
        assert!(serde_json::from_str::<Response>(&missing).is_err());

        let present = format!(
            r#"{{"type":"{kind}","protocol_version":2,"request_id":"r1","revision":"missing"{repair_field},"config":{{"storage":null,"capture_enabled":false,"sites":[],"strip_params":["utm_*","fbclid","gclid"],"near_repeat_secs":300}}}}"#
        );
        assert!(serde_json::from_str::<Response>(&present).is_ok());
    }
}
