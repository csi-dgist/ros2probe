use crate::command::{
    protocol::{TopicListRequest, TopicListResponse},
    state::CommandState,
};

pub fn build_response(request: TopicListRequest, state: CommandState) -> TopicListResponse {
    let topics = state
        .topics
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
    TopicListResponse {
        total_count: topics.len(),
        topics,
    }
}

pub fn is_hidden_topic(name: &str) -> bool {
    name.split('/').any(|seg| seg.starts_with('_') && !seg.is_empty())
}
