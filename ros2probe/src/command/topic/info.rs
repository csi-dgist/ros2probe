use crate::command::{
    protocol::{TopicDetails, TopicInfoRequest, TopicInfoResponse},
    state::CommandState,
};

pub fn build_response(request: TopicInfoRequest, state: CommandState) -> TopicInfoResponse {
    let topic = state
        .topic_details
        .into_iter()
        .find(|topic: &TopicDetails| topic.name == request.topic_name);
    TopicInfoResponse { topic }
}
