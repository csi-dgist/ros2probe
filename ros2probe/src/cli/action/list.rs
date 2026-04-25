use anyhow::bail;
use clap::Args;

use crate::command::protocol::{ActionListRequest, CommandRequest, CommandResponse};

use super::super::bag::util::send_request;

#[derive(Debug, Args)]
pub struct ActionListCommand {
    /// Additionally show the action type
    #[arg(short = 't', long = "show-types")]
    pub show_types: bool,

    /// Only display the number of actions discovered
    #[arg(short = 'c', long = "count-actions")]
    pub count_only: bool,
}

pub fn run(args: ActionListCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::ActionList(ActionListRequest {
        show_types: args.show_types,
        count_only: args.count_only,
    }))?;

    match response {
        CommandResponse::ActionList(response) => {
            if args.count_only {
                println!("{}", response.total_count);
                return Ok(());
            }

            for action in response.actions {
                if args.show_types {
                    println!(
                        "{} [{}]",
                        action.name,
                        action.type_name.unwrap_or_else(|| String::from("-"))
                    );
                } else {
                    println!("{}", action.name);
                }
            }
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for action list request"),
    }
}
