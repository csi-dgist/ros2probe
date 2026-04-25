use anyhow::bail;
use clap::Args;

use crate::command::protocol::{ActionDetails, ActionInfoRequest, CommandRequest, CommandResponse};

use super::super::bag::util::send_request;

#[derive(Debug, Args)]
pub struct ActionInfoCommand {
    /// Name of the ROS action to get info for
    pub action_name: String,

    /// Additionally show the action type
    #[arg(short = 't', long = "show-types")]
    pub show_types: bool,

    /// Only display the number of action clients and action servers
    #[arg(short = 'c', long = "count")]
    pub count_only: bool,
}

pub fn run(args: ActionInfoCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::ActionInfo(ActionInfoRequest {
        action_name: args.action_name.clone(),
    }))?;

    match response {
        CommandResponse::ActionInfo(response) => {
            let Some(action) = response.action else {
                bail!("unknown action '{}'", args.action_name);
            };
            print_action_info(&action, args.show_types, args.count_only);
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for action info request"),
    }
}

fn print_action_info(action: &ActionDetails, show_types: bool, count_only: bool) {
    println!("Action: {}", action.name);
    println!("Action clients: {}", action.clients.len());
    if !count_only {
        for client in &action.clients {
            if show_types {
                println!(
                    "    {} [{}]",
                    client,
                    action.type_name.as_deref().unwrap_or("-")
                );
            } else {
                println!("    {}", client);
            }
        }
    }
    println!("Action servers: {}", action.servers.len());
    if !count_only {
        for server in &action.servers {
            if show_types {
                println!(
                    "    {} [{}]",
                    server,
                    action.type_name.as_deref().unwrap_or("-")
                );
            } else {
                println!("    {}", server);
            }
        }
    }
}
