//! A macOS Yes/No alert that can time out and be canceled.
//!
//! macOS draws the alert in UserNotificationCenter, not in the process that
//! asks for it, so the alert outlives that process unless it is canceled or
//! times out. `CFUserNotificationDisplayAlert`, which `rfd` uses, offers
//! neither once shown. The host forbids unsafe code; the few CoreFoundation
//! calls this needs live here instead. The crate is empty on other platforms.
#![cfg(target_os = "macos")]
#![deny(unsafe_op_in_unsafe_fn)]

use std::time::Duration;

use objc2_core_foundation::{
    CFDictionary, CFOptionFlags, CFRetained, CFString, CFUserNotification,
    kCFUserNotificationAlertHeaderKey, kCFUserNotificationAlertMessageKey,
    kCFUserNotificationAlternateButtonTitleKey, kCFUserNotificationCautionAlertLevel,
    kCFUserNotificationDefaultButtonTitleKey, kCFUserNotificationDefaultResponse,
};

/// The low bits of a response hold which button ended the alert.
const RESPONSE_BUTTON_MASK: CFOptionFlags = 0x3;

pub struct AlertText<'a> {
    pub title: &'a str,
    pub message: &'a str,
    pub yes: &'a str,
    pub no: &'a str,
}

/// A caution-level alert on screen, with `yes` as its default button.
pub struct Alert(CFRetained<CFUserNotification>);

// SAFETY: a CFUserNotification is a handle to an alert that
// UserNotificationCenter owns. Canceling it sends that process a Mach message
// and does not touch the state `wait` blocks on, so `cancel` may run on
// another thread while `wait` is blocked; that is how an alert is withdrawn.
unsafe impl Send for Alert {}
// SAFETY: as above; both methods take `&self` and CoreFoundation serializes
// the calls.
unsafe impl Sync for Alert {}

impl Alert {
    /// Show the alert. It closes by itself, as if declined, after `timeout`.
    /// The error is CoreFoundation's nonzero error code.
    pub fn show(text: AlertText<'_>, timeout: Duration) -> Result<Self, i32> {
        // SAFETY: these are constant CFStrings exported by CoreFoundation,
        // which is always loaded on macOS; reading them has no side effects.
        let keys = unsafe {
            [
                kCFUserNotificationAlertHeaderKey,
                kCFUserNotificationAlertMessageKey,
                kCFUserNotificationDefaultButtonTitleKey,
                kCFUserNotificationAlternateButtonTitleKey,
            ]
        };
        let keys: Vec<&CFString> = keys.into_iter().collect::<Option<_>>().ok_or(-1)?;
        let values = [text.title, text.message, text.yes, text.no].map(CFString::from_str);
        let values: Vec<&CFString> = values.iter().map(|value| &**value).collect();
        let dictionary = CFDictionary::<CFString, CFString>::from_slices(&keys, &values);
        let mut error = 0;
        // SAFETY: `None` selects the default allocator, `error` is a valid
        // out pointer, and the dictionary maps the documented CFString keys
        // to CFString values, as CFUserNotificationCreate requires.
        let notification = unsafe {
            CFUserNotification::new(
                None,
                timeout.as_secs_f64(),
                kCFUserNotificationCautionAlertLevel,
                &mut error,
                Some(dictionary.as_opaque()),
            )
        };
        match notification {
            Some(notification) if error == 0 => Ok(Self(notification)),
            _ => Err(if error == 0 { -1 } else { error }),
        }
    }

    /// Block until the alert closes. True only for a click on `yes`; `no`,
    /// the timeout, a cancel, or any failure is false.
    pub fn wait(&self) -> bool {
        let mut response: CFOptionFlags = 0;
        // SAFETY: `response` is a valid out pointer. A zero timeout waits for
        // the alert's own timeout or an answer.
        let status = unsafe { self.0.receive_response(0.0, &mut response) };
        status == 0 && response & RESPONSE_BUTTON_MASK == kCFUserNotificationDefaultResponse
    }

    /// Take the alert off screen. `wait` then returns false.
    pub fn cancel(&self) {
        self.0.cancel();
    }
}
