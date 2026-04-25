use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context};

use crate::{
    command::protocol::{
        TopicBwStartRequest, TopicBwStartResponse, TopicBwStatusRequest, TopicBwStatusResponse,
        TopicBwStopRequest, TopicBwStopResponse,
    },
    runtime::{RuntimeCommand, RuntimeReply},
};

const RUNTIME_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

pub fn build_start_response(
    request: TopicBwStartRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicBwStartResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicBwStart {
            request,
            reply: reply_tx,
        })
        .context("send topic bw start command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicBwStarted(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic bw start"),
        Err(err) => bail!("timed out waiting for topic bw to start: {err}"),
    }
}

pub fn build_status_response(
    _request: TopicBwStatusRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicBwStatusResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicBwStatus { reply: reply_tx })
        .context("send topic bw status command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicBwStatus(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic bw status"),
        Err(err) => bail!("timed out waiting for topic bw status: {err}"),
    }
}

pub fn build_stop_response(
    _request: TopicBwStopRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicBwStopResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicBwStop { reply: reply_tx })
        .context("send topic bw stop command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicBwStopped(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic bw stop"),
        Err(err) => bail!("timed out waiting for topic bw stop: {err}"),
    }
}
