use crate::command::{
    protocol::{TopicListRequest, TopicListResponse},
    state::CommandState,
};

pub fn build_response(_request: TopicListRequest, state: CommandState) -> TopicListResponse {
    let topics = state
        .topics
        .into_iter()
        .filter(|topic| topic.type_names.iter().any(|t| t.contains("/msg/")))
        .collect::<Vec<_>>();
    TopicListResponse {
        total_count: topics.len(),
        topics,
    }
}

pub fn is_hidden_topic(name: &str) -> bool {
    name.split('/').any(|seg| seg.starts_with('_') && !seg.is_empty())
}
