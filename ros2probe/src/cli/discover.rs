use anyhow::bail;
use clap::{Args, ValueEnum};

use crate::{
    client::send_request,
    command::protocol::{CommandRequest, CommandResponse, DiscoverMode, DiscoverRequest},
};

#[derive(Debug, Args)]
pub struct DiscoverCommand {
    /// Discovery refresh mode to run.
    #[arg(long, value_enum, default_value_t = DiscoverModeArg::Auto)]
    pub mode: DiscoverModeArg,
    /// Run only the RTPS /ros_discovery_info refresh.
    #[arg(long)]
    pub rtps: bool,
    /// Run only the Zenoh liveliness refresh.
    #[arg(long)]
    pub zenoh: bool,
    /// Run both RTPS and Zenoh refresh paths.
    #[arg(long)]
    pub all: bool,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum DiscoverModeArg {
    #[default]
    Auto,
    Rtps,
    Zenoh,
    All,
}

impl From<DiscoverModeArg> for DiscoverMode {
    fn from(mode: DiscoverModeArg) -> Self {
        match mode {
            DiscoverModeArg::Auto => DiscoverMode::Auto,
            DiscoverModeArg::Rtps => DiscoverMode::Rtps,
            DiscoverModeArg::Zenoh => DiscoverMode::Zenoh,
            DiscoverModeArg::All => DiscoverMode::All,
        }
    }
}

pub fn run(args: DiscoverCommand) -> anyhow::Result<()> {
    let mode = select_mode(&args)?;
    let request = DiscoverRequest::from_current_env(mode);
    let response = send_request(CommandRequest::Discover(request))?;
    match response {
        CommandResponse::Discover(response) => {
            println!("Discovery triggered.");
            for message in response.messages {
                println!("{message}");
            }
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for discover request"),
    }
}

fn select_mode(args: &DiscoverCommand) -> anyhow::Result<DiscoverMode> {
    let flag_count = usize::from(args.rtps) + usize::from(args.zenoh) + usize::from(args.all);
    if flag_count > 1 {
        bail!("use only one of --rtps, --zenoh, or --all");
    }
    if args.rtps {
        Ok(DiscoverMode::Rtps)
    } else if args.zenoh {
        Ok(DiscoverMode::Zenoh)
    } else if args.all {
        Ok(DiscoverMode::All)
    } else {
        Ok(args.mode.into())
    }
}
