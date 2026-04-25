use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::{
    command::protocol::{
        TopicHzBwStatusResponse, TopicHzStartRequest, TopicHzStartResponse, TopicHzStatusRequest,
        TopicHzStatusResponse, TopicHzStopRequest, TopicHzStopResponse,
    },
    runtime::{RuntimeCommand, RuntimeReply},
};

const RUNTIME_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

pub fn build_hz_bw_status_response(
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicHzBwStatusResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicHzBwStatus { reply: reply_tx })
        .context("send topic hz+bw status command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicHzBwStatus(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic hz+bw status"),
        Err(err) => bail!("timed out waiting for topic hz+bw status: {err}"),
    }
}

pub fn build_start_response(
    request: TopicHzStartRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicHzStartResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicHzStart {
            request,
            reply: reply_tx,
        })
        .context("send topic hz start command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicHzStarted(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic hz start"),
        Err(err) => bail!("timed out waiting for topic hz to start: {err}"),
    }
}

pub fn build_status_response(
    _request: TopicHzStatusRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicHzStatusResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicHzStatus { reply: reply_tx })
        .context("send topic hz status command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicHzStatus(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic hz status"),
        Err(err) => bail!("timed out waiting for topic hz status: {err}"),
    }
}

pub fn build_stop_response(
    _request: TopicHzStopRequest,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) -> anyhow::Result<TopicHzStopResponse> {
    let (reply_tx, reply_rx) = mpsc::channel();
    runtime_command_tx
        .send(RuntimeCommand::TopicHzStop { reply: reply_tx })
        .context("send topic hz stop command to runtime")?;

    match reply_rx.recv_timeout(RUNTIME_COMMAND_TIMEOUT) {
        Ok(RuntimeReply::TopicHzStopped(response)) => Ok(response),
        Ok(RuntimeReply::Error(message)) => bail!(message),
        Ok(_) => bail!("unexpected runtime reply for topic hz stop"),
        Err(err) => bail!("timed out waiting for topic hz stop: {err}"),
    }
}
