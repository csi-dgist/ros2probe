use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::{
    command::protocol::{
        TopicDelayStartRequest, TopicDelayStartResponse, TopicDelayStatusRequest,
        TopicDelayStatusResponse, TopicDelayStopRequest, TopicDelayStopResponse,
    },
    runtime::{RuntimeCommand, RuntimeReply},
};

const RUNTIME_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

pub fn build_start_response(
    request: TopicDelayStartRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicDelayStartResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicDelayStart {
            request,
            reply: reply_tx,
        })
        .context("send topic delay start command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicDelayStarted(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic delay start"),
        Err(err) => bail!("timed out waiting for topic delay to start: {err}"),
    }
}

pub fn build_status_response(
    _request: TopicDelayStatusRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicDelayStatusResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicDelayStatus { reply: reply_tx })
        .context("send topic delay status command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicDelayStatus(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic delay status"),
        Err(err) => bail!("timed out waiting for topic delay status: {err}"),
    }
}

pub fn build_stop_response(
    _request: TopicDelayStopRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicDelayStopResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicDelayStop { reply: reply_tx })
        .context("send topic delay stop command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicDelayStopped(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic delay stop"),
        Err(err) => bail!("timed out waiting for topic delay stop: {err}"),
    }
}
