use crate::command::{
    protocol::{ActionListRequest, ActionListResponse},
    state::CommandState,
};

pub fn build_response(_request: ActionListRequest, state: CommandState) -> ActionListResponse {
    ActionListResponse {
        total_count: state.actions.len(),
        actions: state.actions,
    }
}
