mod cli;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(name = "tuyau", version, about = "Declarative PipeWire patchbay")]
struct Arguments {
    /// Config file [default: $TUYAU_CONFIG or $XDG_CONFIG_HOME/tuyau/config.toml]
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// GUI state file [default: $XDG_STATE_HOME/tuyau/state.toml]
    #[arg(long, global = true)]
    state: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<cli::Command>,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let arguments = Arguments::parse();
    let paths = cli::Paths {
        config: arguments
            .config
            .unwrap_or_else(tuyau_core::Config::default_path),
        state: arguments
            .state
            .unwrap_or_else(tuyau_core::State::default_path),
    };
    match arguments.command {
        None | Some(cli::Command::Gui) => tuyau_gui::run(tuyau_gui::Options {
            config_path: paths.config,
            state_path: paths.state,
        }),
        Some(command) => cli::run(command, &paths),
    }
}
