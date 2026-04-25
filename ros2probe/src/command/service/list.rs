use crate::command::{
    protocol::{ServiceInfo, ServiceListRequest, ServiceListResponse},
    state::CommandState,
};

pub fn build_response(request: ServiceListRequest, state: CommandState) -> ServiceListResponse {
    let mut services = state.services;

    if request.include_hidden {
        for action in &state.actions {
            let type_name = action.type_name.clone().unwrap_or_default();
            for suffix in ["/_action/send_goal", "/_action/cancel_goal", "/_action/get_result"] {
                services.push(ServiceInfo {
                    name: format!("{}{suffix}", action.name),
                    type_name: type_name.clone(),
                });
            }
        }
        services.sort_by(|a, b| a.name.cmp(&b.name));
    }

    ServiceListResponse {
        total_count: services.len(),
        services,
    }
}
