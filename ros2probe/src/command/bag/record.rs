use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::{
    runtime::{RuntimeCommand, RuntimeReply},
    command::protocol::{BagRecordRequest, BagRecordResponse},
};

const RUNTIME_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

pub fn build_response(
    request: BagRecordRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<BagRecordResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::BagRecord {
            request,
            reply: reply_tx,
        })
        .context("send bag record command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::BagStarted(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for bag record"),
        Err(err) => bail!("timed out waiting for bag recorder to start: {err}"),
    }
}
