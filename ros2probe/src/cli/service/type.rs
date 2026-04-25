use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{CommandRequest, CommandResponse, ServiceTypeRequest},
};

#[derive(Debug, Args)]
pub struct ServiceTypeCommand {
    /// Name of the service to inspect
    pub service_name: String,
}

pub fn run(args: ServiceTypeCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::ServiceType(ServiceTypeRequest {
        service_name: args.service_name.clone(),
    }))?;

    match response {
        CommandResponse::ServiceType(response) => {
            if response.type_names.is_empty() {
                bail!("unknown service '{}'", args.service_name);
            }
            for type_name in response.type_names {
                println!("{type_name}");
            }
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for service type request"),
    }
}
