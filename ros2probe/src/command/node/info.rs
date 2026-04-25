use crate::command::{
    protocol::{NodeInfoRequest, NodeInfoResponse},
    state::CommandState,
};

pub fn build_response(request: NodeInfoRequest, state: CommandState) -> NodeInfoResponse {
    let node = state
        .node_details
        .into_iter()
        .find(|node| full_node_name(node.namespace.as_str(), node.name.as_str()) == request.node_name);

    NodeInfoResponse { node }
}

fn full_node_name(namespace: &str, name: &str) -> String {
    if namespace == "/" {
        format!("/{name}")
    } else {
        format!("{namespace}/{name}")
    }
}
