use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{
        CommandRequest, CommandResponse, TopicDetails, TopicEndpointInfo, TopicInfoRequest,
    },
};

#[derive(Debug, Args)]
pub struct TopicInfoCommand {
    /// Name of the ROS topic to get info
    pub topic_name: String,

    /// Print detailed publisher and subscriber information
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}

pub fn run(args: TopicInfoCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::TopicInfo(TopicInfoRequest {
        topic_name: args.topic_name.clone(),
    }))?;

    match response {
        CommandResponse::TopicInfo(response) => {
            let Some(topic) = response.topic else {
                bail!("unknown topic '{}'", args.topic_name);
            };
            print_topic_info(&topic, args.verbose);
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic info request"),
    }
}

fn print_topic_info(topic: &TopicDetails, verbose: bool) {
    println!("Type: {}", format_types(&topic.type_names));
    if verbose {
        println!();
    }
    println!("Publisher count: {}", topic.publisher_count);

    if verbose && !topic.publishers.is_empty() {
        println!();
        for endpoint in &topic.publishers {
            print_endpoint(endpoint);
        }
    }

    println!("Subscription count: {}", topic.subscription_count);

    if verbose && !topic.subscriptions.is_empty() {
        println!();
        for endpoint in &topic.subscriptions {
            print_endpoint(endpoint);
        }
    }
}

fn print_endpoint(endpoint: &TopicEndpointInfo) {
    if let Some(node_name) = &endpoint.node_name {
        println!("Node name: {node_name}");
    }
    if let Some(node_namespace) = &endpoint.node_namespace {
        println!("Node namespace: {node_namespace}");
    }
    if let Some(topic_type) = &endpoint.topic_type {
        println!("Topic type: {topic_type}");
    }
    println!("Endpoint type: {}", endpoint.endpoint_type);
    println!("GID: {}", endpoint.gid);
    if endpoint.reliability.is_some()
        || endpoint.history.is_some()
        || endpoint.durability.is_some()
        || endpoint.lifespan.is_some()
        || endpoint.deadline.is_some()
        || endpoint.liveliness.is_some()
        || endpoint.liveliness_lease_duration.is_some()
    {
        println!("QoS profile:");
        if let Some(value) = &endpoint.reliability {
            println!("  Reliability: {value}");
        }
        if let Some(value) = &endpoint.history {
            println!("  History (Depth): {value}");
        }
        if let Some(value) = &endpoint.durability {
            println!("  Durability: {value}");
        }
        if let Some(value) = &endpoint.lifespan {
            println!("  Lifespan: {value}");
        }
        if let Some(value) = &endpoint.deadline {
            println!("  Deadline: {value}");
        }
        if let Some(value) = &endpoint.liveliness {
            println!("  Liveliness: {value}");
        }
        if let Some(value) = &endpoint.liveliness_lease_duration {
            println!("  Liveliness lease duration: {value}");
        }
    }
    println!();
}

fn format_types(type_names: &[String]) -> String {
    if type_names.is_empty() {
        String::from("-")
    } else {
        type_names.join(", ")
    }
}
