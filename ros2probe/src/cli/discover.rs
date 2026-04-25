use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{CommandRequest, CommandResponse, DiscoverRequest},
};

#[derive(Debug, Args)]
pub struct DiscoverCommand;

pub fn run(_args: DiscoverCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::Discover(DiscoverRequest))?;
    match response {
        CommandResponse::Discover(_) => {
            println!("Discovery triggered.");
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for discover request"),
    }
}
