use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::{
    command::protocol::{BagSetPausedRequest, BagSetPausedResponse},
    runtime::{RuntimeCommand, RuntimeReply},
};

const RUNTIME_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

pub fn build_response(
    request: BagSetPausedRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<BagSetPausedResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::BagSetPaused {
            request,
            reply: reply_tx,
        })
        .context("send bag pause command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::BagSetPaused(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for bag pause"),
        Err(err) => bail!("timed out waiting for bag pause: {err}"),
    }
}
