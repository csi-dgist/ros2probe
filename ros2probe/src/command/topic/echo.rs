use std::sync::mpsc;
use anyhow::{Context, bail};

use crate::{
    command::protocol::{
        TopicEchoStartRequest, TopicEchoStartResponse, TopicEchoStatusRequest,
        TopicEchoStatusResponse, TopicEchoStopRequest, TopicEchoStopResponse,
    },
    runtime::{RuntimeCommand, RuntimeReply},
};

const START_STOP_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn build_start_response(
    request: TopicEchoStartRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicEchoStartResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicEchoStart {
            request,
            reply: reply_tx,
        })
        .context("send topic echo start command to runtime")?;

    match reply_rx.recv_timeout(START_STOP_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicEchoStarted(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic echo start"),
        Err(err) => bail!("timed out waiting for topic echo to start: {err}"),
    }
}

pub fn build_status_response(
    _request: TopicEchoStatusRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicEchoStatusResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicEchoStatus { reply: reply_tx })
        .context("send topic echo status command to runtime")?;

    match reply_rx.recv() {
        Ok(RuntimeReply::TopicEchoStatus(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic echo status"),
        Err(err) => bail!("waiting for topic echo status failed: {err}"),
    }
}

pub fn build_stop_response(
    _request: TopicEchoStopRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicEchoStopResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicEchoStop { reply: reply_tx })
        .context("send topic echo stop command to runtime")?;

    match reply_rx.recv_timeout(START_STOP_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicEchoStopped(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic echo stop"),
        Err(err) => bail!("timed out waiting for topic echo stop: {err}"),
    }
}
