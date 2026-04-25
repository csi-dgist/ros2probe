use crate::command::{
    protocol::{ActionDetails, ActionInfoRequest, ActionInfoResponse},
    state::CommandState,
};

pub fn build_response(request: ActionInfoRequest, state: CommandState) -> ActionInfoResponse {
    let action = state
        .action_details
        .into_iter()
        .find(|action: &ActionDetails| action.name == request.action_name);
    ActionInfoResponse { action }
}
