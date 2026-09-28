//! Shows one confirmation that times out after the given number of seconds and
//! prints `accepted` or `declined`. Used by `scripts/check-macos-alert.mjs` to
//! check the timeout path without waiting for the host's full timeout.
//! Usage: show_alert SECONDS

#[cfg(target_os = "macos")]
fn main() {
    use brauser_macos_alert::{Alert, AlertText};
    use std::time::Duration;

    let seconds: f64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .expect("usage: show_alert SECONDS");
    let text = AlertText {
        title: "Brauser alert check",
        message: "This closes by itself. Please do not click.",
        yes: "Yes",
        no: "No",
    };
    let alert = Alert::show(text, Duration::from_secs_f64(seconds)).expect("showing the alert");
    println!("{}", if alert.wait() { "accepted" } else { "declined" });
}

#[cfg(not(target_os = "macos"))]
fn main() {}
