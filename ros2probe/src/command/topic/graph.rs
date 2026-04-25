use crate::command::{
    protocol::{TopicGraphRequest, TopicGraphResponse},
    state::CommandState,
    topic::list::is_hidden_topic,
};

pub fn build_response(request: TopicGraphRequest, state: CommandState) -> TopicGraphResponse {
    let total_topics_count = state.topic_details.len();
    let total_nodes_count = state.node_details.len();
    let total_actions_count = state.actions.len();
    let total_services_count = state.services.len();
    let topics = state
        .topic_details
        .into_iter()
        .filter(|topic| {
            if request.include_hidden {
                true
            } else {
                !is_hidden_topic(&topic.name)
                    && topic.type_names.iter().any(|t| t.contains("/msg/"))
            }
        })
        .collect::<Vec<_>>();
    TopicGraphResponse {
        topics,
        nodes: state.node_details,
        total_topics_count,
        total_nodes_count,
        total_actions_count,
        total_services_count,
    }
}
