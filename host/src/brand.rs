//! Product naming for the host. The extension's copy is `extension/brand.ts`.

/// Lowercase namespace for markers, frontmatter keys, temp files, and the binary.
/// A macro so `concat!` can build compile-time marker strings from it.
#[macro_export]
macro_rules! namespace {
    () => {
        "brauser"
    };
}

/// The user-facing product name, as a macro for compile-time messages.
#[macro_export]
macro_rules! app_name {
    () => {
        "Brauser"
    };
}

pub const APP_NAME: &str = app_name!();
pub const NAMESPACE: &str = namespace!();
pub const BLOCK_START: &str = concat!("<!-- ", namespace!(), ":start -->");
pub const BLOCK_END: &str = concat!("<!-- ", namespace!(), ":end -->");
pub const FRONTMATTER_KEY: &str = concat!(namespace!(), ":");
/// Names the scripted-answer directory in `scripted-dialogs` test builds.
#[cfg(feature = "scripted-dialogs")]
pub const SCRIPTED_DIALOGS_ENV: &str = "BRAUSER_SCRIPTED_DIALOGS";
