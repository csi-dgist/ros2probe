use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{CommandRequest, CommandResponse, TopicTypeRequest},
};

#[derive(Debug, Args)]
pub struct TopicTypeCommand {
    /// Name of the topic to inspect
    pub topic_name: String,
}

pub fn run(args: TopicTypeCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::TopicType(TopicTypeRequest {
        topic_name: args.topic_name.clone(),
    }))?;

    match response {
        CommandResponse::TopicType(response) => {
            if response.type_names.is_empty() {
                bail!("unknown topic '{}'", args.topic_name);
            }
            for type_name in response.type_names {
                println!("{type_name}");
            }
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic type request"),
    }
}
