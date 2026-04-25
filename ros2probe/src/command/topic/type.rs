use crate::command::{
    protocol::{TopicTypeRequest, TopicTypeResponse},
    state::CommandState,
};

pub fn build_response(request: TopicTypeRequest, state: CommandState) -> TopicTypeResponse {
    let type_names = state
        .topics
        .into_iter()
        .find(|topic| topic.name == request.topic_name)
        .map(|topic| topic.type_names)
        .unwrap_or_default();
    TopicTypeResponse { type_names }
}
