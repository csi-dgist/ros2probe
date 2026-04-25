use crate::command::{
    protocol::{NodeListResponse, NodeListRequest},
    state::CommandState,
};

pub fn build_response(_request: NodeListRequest, state: CommandState) -> NodeListResponse {
    NodeListResponse {
        total_count: state.nodes.len(),
        nodes: state.nodes,
    }
}
