use crate::command::{
    protocol::{ServiceTypeRequest, ServiceTypeResponse},
    state::CommandState,
};

pub fn build_response(request: ServiceTypeRequest, state: CommandState) -> ServiceTypeResponse {
    let type_names = state
        .services
        .into_iter()
        .filter(|service| service.name == request.service_name)
        .map(|service| service.type_name)
        .collect();
    ServiceTypeResponse { type_names }
}
