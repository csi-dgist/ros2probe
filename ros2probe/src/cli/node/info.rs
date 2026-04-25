use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{
        CommandRequest, CommandResponse, NodeDetails, NodeEndpointSummary, NodeInfoRequest,
        NodeServiceInfo,
    },
};

#[derive(Debug, Args)]
pub struct NodeInfoCommand {
    /// Name of the ROS node to get info for
    pub node_name: String,
}

pub fn run(args: NodeInfoCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::NodeInfo(NodeInfoRequest {
        node_name: args.node_name.clone(),
    }))?;

    match response {
        CommandResponse::NodeInfo(response) => {
            let Some(node) = response.node else {
                bail!("unknown node '{}'", args.node_name);
            };
            print_node_info(&node);
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for node info request"),
    }
}

fn print_node_info(node: &NodeDetails) {
    println!("{}", full_node_name(node.namespace.as_str(), node.name.as_str()));
    println!("  Subscribers:");
    for endpoint in &node.subscribers {
        print_endpoint_summary(endpoint);
    }
    println!("  Publishers:");
    for endpoint in &node.publishers {
        print_endpoint_summary(endpoint);
    }
    println!("  Service Servers:");
    for service in &node.service_servers {
        print_service_summary(service);
    }
    println!("  Service Clients:");
    for service in &node.service_clients {
        print_service_summary(service);
    }
    println!("  Action Servers:");
    println!("  Action Clients:");
}

fn print_endpoint_summary(endpoint: &NodeEndpointSummary) {
    println!("    {}: {}", endpoint.name, endpoint.type_name);
}

fn print_service_summary(service: &NodeServiceInfo) {
    println!("    {}: {}", service.name, service.type_name);
}

fn full_node_name(namespace: &str, name: &str) -> String {
    if namespace == "/" {
        format!("/{name}")
    } else {
        format!("{namespace}/{name}")
    }
}
