use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{CommandRequest, CommandResponse, ServiceListRequest},
};

#[derive(Debug, Args)]
pub struct ServiceListCommand {
    /// Additionally show the service type
    #[arg(short = 't', long = "show-types")]
    pub show_types: bool,

    /// Only display the number of services discovered
    #[arg(short = 'c', long = "count-services")]
    pub count_only: bool,

    /// Include hidden services (action sub-services)
    #[arg(short = 'a', long = "include-hidden-services")]
    pub include_hidden: bool,
}

pub fn run(args: ServiceListCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::ServiceList(ServiceListRequest {
        show_types: args.show_types,
        count_only: args.count_only,
        include_hidden: args.include_hidden,
    }))?;

    match response {
        CommandResponse::ServiceList(response) => {
            if args.count_only {
                println!("{}", response.total_count);
                return Ok(());
            }

            for service in response.services {
                if args.show_types {
                    println!("{} [{}]", service.name, service.type_name);
                } else {
                    println!("{}", service.name);
                }
            }
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for service list request"),
    }
}
