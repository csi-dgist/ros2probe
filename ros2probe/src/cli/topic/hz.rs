use anyhow::{bail, Context};
use clap::Args;
use tokio::runtime::Runtime;

use crate::command::protocol::{
    CommandRequest, CommandResponse, TopicHzStartRequest, TopicHzStatusRequest, TopicHzStopRequest,
};

use super::super::bag::util::send_request;

#[derive(Debug, Args)]
pub struct TopicHzCommand {
    /// Name of the ROS topic to observe
    pub topic_name: String,

    /// Window size, in number of messages, for calculating rate
    #[arg(short = 'w', long = "window", default_value_t = 10_000)]
    pub window: usize,
}

pub fn run(args: TopicHzCommand) -> anyhow::Result<()> {
    if args.window == 0 {
        bail!("--window must be greater than 0");
    }

    let response = send_request(CommandRequest::TopicHzStart(TopicHzStartRequest {
        topic_name: args.topic_name.clone(),
        window_size: args.window,
    }))?;

    match response {
        CommandResponse::TopicHzStart(response) => {
            let _ = response;
            wait_for_samples()?;
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic hz start request"),
    }
}

fn wait_for_samples() -> anyhow::Result<()> {
    let runtime = Runtime::new().context("create ctrl-c runtime")?;
    runtime.block_on(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        interval.tick().await;
        loop {
            tokio::select! {
                result = &mut ctrl_c => {
                    result.context("wait for ctrl-c")?;
                    match send_request(CommandRequest::TopicHzStop(TopicHzStopRequest))? {
                        CommandResponse::TopicHzStop(response) => {
                            let _ = response;
                            return Ok(());
                        }
                        CommandResponse::Error(error) => bail!(error.message),
                        _ => bail!("unexpected response for topic hz stop request"),
                    }
                }
                _ = interval.tick() => {
                    match send_request(CommandRequest::TopicHzStatus(TopicHzStatusRequest))? {
                        CommandResponse::TopicHzStatus(response) => print_status(response),
                        CommandResponse::Error(error) => bail!(error.message),
                        _ => bail!("unexpected response for topic hz status request"),
                    }
                }
            }
        }
    })
}

fn print_status(response: crate::command::protocol::TopicHzStatusResponse) {
    if !response.active {
        return;
    }
    if response.last_message_secs_ago.unwrap_or(f64::INFINITY) > 2.0 {
        return;
    }

    let (Some(average_rate_hz), Some(min_period_seconds), Some(max_period_seconds), Some(std_dev_seconds)) = (
        response.average_rate_hz,
        response.min_period_seconds,
        response.max_period_seconds,
        response.std_dev_seconds,
    ) else {
        return;
    };

    println!("average rate: {:.3}", average_rate_hz);
    println!(
        "\tmin: {:.3}s max: {:.3}s std dev: {:.5}s window: {}",
        min_period_seconds,
        max_period_seconds,
        std_dev_seconds,
        response.window,
    );
}
