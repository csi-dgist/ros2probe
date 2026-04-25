use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::{
    runtime::{RuntimeCommand, RuntimeReply},
    command::protocol::{BagStopRequest, BagStopResponse},
};

const RUNTIME_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

pub fn build_response(
    _request: BagStopRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<BagStopResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::BagStop { reply: reply_tx })
        .context("send bag stop command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::BagStopped(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for bag stop"),
        Err(err) => bail!("timed out waiting for bag stop: {err}"),
    }
}
