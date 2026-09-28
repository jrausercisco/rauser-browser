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
        None | Some("serve") => native::serve(ConfigStore::load()?),
        Some("--version") => {
            println!("{NAMESPACE} {}", env!("CARGO_PKG_VERSION"));
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
        Some(origin) if origin.starts_with("chrome-extension://") => {
            native::serve(ConfigStore::load()?)
        }
        Some(other) => bail!("unknown command or browser origin: {other}"),
    }
}
