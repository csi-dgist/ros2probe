use anyhow::{bail, Context};
use clap::Args;
use tokio::runtime::Runtime;

use crate::command::protocol::{
    CommandRequest, CommandResponse, TopicBwStartRequest, TopicBwStatusRequest, TopicBwStopRequest,
};

use super::super::bag::util::send_request;

#[derive(Debug, Args)]
pub struct TopicBwCommand {
    /// Topic name to monitor for bandwidth utilization
    pub topic_name: String,

    /// Maximum window size, in number of messages, for calculating rate
    #[arg(short = 'w', long = "window", default_value_t = 100)]
    pub window: usize,
}

pub fn run(args: TopicBwCommand) -> anyhow::Result<()> {
    if args.window == 0 {
        bail!("--window must be greater than 0");
    }

    let response = send_request(CommandRequest::TopicBwStart(TopicBwStartRequest {
        topic_name: args.topic_name.clone(),
        window_size: args.window,
    }))?;

    match response {
        CommandResponse::TopicBwStart(response) => {
            let _ = response;
            wait_for_samples()?;
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic bw start request"),
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
                    match send_request(CommandRequest::TopicBwStop(TopicBwStopRequest))? {
                        CommandResponse::TopicBwStop(_) => return Ok(()),
                        CommandResponse::Error(error) => bail!(error.message),
                        _ => bail!("unexpected response for topic bw stop request"),
                    }
                }
                _ = interval.tick() => {
                    match send_request(CommandRequest::TopicBwStatus(TopicBwStatusRequest))? {
                        CommandResponse::TopicBwStatus(response) => print_status(response),
                        CommandResponse::Error(error) => bail!(error.message),
                        _ => bail!("unexpected response for topic bw status request"),
                    }
                }
            }
        }
    })
}

fn print_status(response: crate::command::protocol::TopicBwStatusResponse) {
    if !response.active {
        return;
    }
    if response.last_message_secs_ago.unwrap_or(f64::INFINITY) > 2.0 {
        return;
    }

    let (Some(bytes_per_second), Some(mean_size_bytes), Some(min_size_bytes), Some(max_size_bytes)) = (
        response.bytes_per_second,
        response.mean_size_bytes,
        response.min_size_bytes,
        response.max_size_bytes,
    ) else {
        return;
    };

    println!(
        "{} from {} messages",
        format_rate(bytes_per_second),
        response.message_count,
    );
    println!(
        "\tMessage size mean: {} min: {} max: {}",
        format_bytes(mean_size_bytes),
        format_bytes(min_size_bytes as f64),
        format_bytes(max_size_bytes as f64),
    );
}

fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second))
}

fn format_bytes(bytes: f64) -> String {
    if bytes >= 1_000_000.0 {
        format!("{:.2} MB", bytes / 1_000_000.0)
    } else if bytes >= 1000.0 {
        format!("{:.2} KB", bytes / 1000.0)
    } else {
        format!("{:.0} B", bytes)
    }
}
