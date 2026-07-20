use anyhow::{Context, bail};
use clap::Args;

use super::super::bag::util::send_request;
use crate::command::protocol::{
    CommandRequest, CommandResponse, TopicDelayStartRequest, TopicDelayStatusRequest,
    TopicDelayStopRequest,
};

#[derive(Debug, Args)]
pub struct TopicDelayCommand {
    /// Topic name to calculate the delay for
    pub topic_name: String,

    /// Window size, in number of messages, for calculating rate
    #[arg(short = 'w', long = "window", default_value_t = 10_000)]
    pub window: usize,
}

pub fn run(args: TopicDelayCommand) -> anyhow::Result<()> {
    if args.window == 0 {
        bail!("--window must be greater than 0");
    }

    let response = send_request(CommandRequest::TopicDelayStart(TopicDelayStartRequest {
        topic_name: args.topic_name.clone(),
        window_size: args.window,
    }))?;

    match response {
        CommandResponse::TopicDelayStart(_) => {
            let result = wait_for_stats();
            let stop_result = send_request(CommandRequest::TopicDelayStop(TopicDelayStopRequest));
            match (result, stop_result) {
                (Ok(()), Ok(CommandResponse::TopicDelayStop(_))) => Ok(()),
                (Ok(()), Ok(CommandResponse::Error(error))) => bail!(error.message),
                (Ok(()), Ok(_)) => bail!("unexpected response for topic delay stop request"),
                (Ok(()), Err(err)) => Err(err),
                (Err(err), _) => Err(err),
            }
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic delay start request"),
    }
}

fn wait_for_stats() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create ctrl-c runtime")?;
    runtime.block_on(async {
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        interval.tick().await;
        loop {
            tokio::select! {
                result = &mut ctrl_c => {
                    result.context("wait for ctrl-c")?;
                    return Ok(());
                }
                _ = interval.tick() => {
                    match send_request(CommandRequest::TopicDelayStatus(TopicDelayStatusRequest))? {
                        CommandResponse::TopicDelayStatus(response) => {
                            if !response.active {
                                return Ok(());
                            }
                            if response.clock_mismatch {
                                println!("header.stamp clock does not match capture wall clock");
                                return Ok(());
                            }
                            if response.missing_header {
                                println!("msg does not have header");
                                return Ok(());
                            }
                            if let Some(stats) = response.stats {
                                print_stats(
                                    stats.avg_ms / 1_000.0,
                                    stats.min_ms / 1_000.0,
                                    stats.max_ms / 1_000.0,
                                    stats.std_dev_ms / 1_000.0,
                                    stats.window,
                                );
                            }
                        }
                        CommandResponse::Error(error) => bail!(error.message),
                        _ => bail!("unexpected response for topic delay status request"),
                    }
                }
            }
        }
    })
}

fn print_stats(
    average_seconds: f64,
    min_seconds: f64,
    max_seconds: f64,
    std_dev_seconds: f64,
    window: usize,
) {
    println!("average delay: {:.3}", average_seconds);
    println!(
        "\tmin: {:.3}s max: {:.3}s std dev: {:.5}s window: {}",
        min_seconds, max_seconds, std_dev_seconds, window,
    );
}
