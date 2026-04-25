use crate::command::protocol::{DiscoverRequest, DiscoverResponse};

pub fn build_response(_request: DiscoverRequest) -> DiscoverResponse {
    crate::shadow::discover::run_discovery();
    DiscoverResponse { triggered: true }
}
