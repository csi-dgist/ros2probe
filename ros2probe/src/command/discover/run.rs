use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::{
    command::protocol::{DiscoverRequest, DiscoverResponse},
    runtime::{RuntimeCommand, RuntimeReply},
};

const RUNTIME_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub fn build_response(
    request: DiscoverRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<DiscoverResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::Discover {
            request,
            reply: reply_tx,
        })
        .context("send discover command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::Discover(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for discover"),
        Err(err) => bail!("timed out waiting for discover: {err}"),
    }
}
