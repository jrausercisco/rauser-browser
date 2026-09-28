#![forbid(unsafe_code)]

use anyhow::{Result, bail};
use brauser_host::{brand::NAMESPACE, config::ConfigStore, dialog, native};

fn main() {
    if let Err(error) = run() {
        eprintln!("{NAMESPACE}: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("serve") => serve(),
        Some("--version") => {
            // Test runners check this marker before relying on scripted dialogs.
            let variant = if cfg!(feature = "scripted-dialogs") {
                " (scripted dialogs)"
            } else {
                ""
            };
            println!("{NAMESPACE} {}{variant}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--config-path") => {
            println!("{}", ConfigStore::default_path()?.display());
            Ok(())
        }
        // Chrome supplies its extension origin as a command-line argument to
        // native messaging hosts. The installed manifest, rather than argv,
        // authorizes the stable extension ID.
        Some(command @ (dialog::PICK_FOLDER_COMMAND | dialog::CONFIRM_COMMAND)) => {
            dialog::run_child_command(command)
        }
        Some(origin) if origin.starts_with("chrome-extension://") => serve(),
        Some(other) => bail!("unknown command or browser origin: {other}"),
    }
}

fn serve() -> Result<()> {
    let config = ConfigStore::load()?;
    // A host killed mid-save can leave a temporary behind; nothing else
    // removes it, so clear old ones before serving.
    config.sweep_stale_temporaries();
    native::serve(config)
}
