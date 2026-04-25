use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{CommandRequest, CommandResponse, ServiceInfo, ServiceListRequest},
};

#[derive(Debug, Args)]
pub struct ServiceFindCommand {
    /// Name of the ROS service type to filter for
    pub service_type: String,

    /// Only display the number of services discovered
    #[arg(short = 'c', long = "count-services")]
    pub count_only: bool,
}

pub fn run(args: ServiceFindCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::ServiceList(ServiceListRequest {
        show_types: false,
        count_only: false,
        include_hidden: false,
    }))?;

    match response {
        CommandResponse::ServiceList(response) => {
            let services = response
                .services
                .into_iter()
                .filter(|service| matches_type(service, &args.service_type))
                .collect::<Vec<_>>();

            if args.count_only {
                println!("{}", services.len());
                return Ok(());
            }

            for service in services {
                println!("{}", service.name);
            }
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for service find request"),
    }
}

fn matches_type(service: &ServiceInfo, service_type: &str) -> bool {
    service.type_name == service_type
}
