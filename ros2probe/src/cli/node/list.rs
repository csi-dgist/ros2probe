use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{CommandRequest, CommandResponse, NodeListRequest},
};

#[derive(Debug, Args)]
pub struct NodeListCommand {
    /// Only display the number of nodes discovered
    #[arg(short = 'c', long = "count-nodes")]
    pub count_only: bool,
}

pub fn run(args: NodeListCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::NodeList(NodeListRequest {
        count_only: args.count_only,
    }))?;

    match response {
        CommandResponse::NodeList(response) => {
            if args.count_only {
                println!("{}", response.total_count);
                return Ok(());
            }

            for node in response.nodes {
                println!("{}{}", node.namespace, node.name);
            }
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for node list request"),
    }
}
