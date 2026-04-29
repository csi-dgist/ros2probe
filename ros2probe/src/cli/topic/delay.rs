use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, bail};
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::command::protocol::{
    CommandRequest, CommandResponse, TopicDelayMessage, TopicDelayStartRequest,
    TopicDelayStatusRequest, TopicDelayStopRequest, TopicTypeRequest,
};

use super::super::bag::util::send_request;

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

    let resolved_type = resolve_message_type(&args.topic_name)?;
    let response = send_request(CommandRequest::TopicDelayStart(TopicDelayStartRequest {
        topic_name: args.topic_name.clone(),
        window_size: args.window,
    }))?;

    match response {
        CommandResponse::TopicDelayStart(_) => {
            let mut extractor = PythonDelayExtractor::spawn()?;
            let result = wait_for_messages(&resolved_type, args.window, &mut extractor);
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

fn wait_for_messages(
    resolved_type: &str,
    window_size: usize,
    extractor: &mut PythonDelayExtractor,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create ctrl-c runtime")?;
    runtime.block_on(async {
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        interval.tick().await;
        let mut delays = DelayWindow::new(window_size);
        loop {
            tokio::select! {
                result = &mut ctrl_c => {
                    result.context("wait for ctrl-c")?;
                    return Ok(());
                }
                _ = interval.tick() => {
                    match send_request(CommandRequest::TopicDelayStatus(TopicDelayStatusRequest))? {
                        CommandResponse::TopicDelayStatus(response) => {
                            let mut updated = false;
                            for message in response.messages {
                                let effective_type = message.type_name.as_deref().unwrap_or(resolved_type);
                                match extractor.extract_delay_seconds(effective_type, &message)? {
                                    DelayOutcome::Delay(delay_seconds) => {
                                        delays.push(delay_seconds);
                                        updated = true;
                                    }
                                    DelayOutcome::MissingHeader => {
                                        println!("msg does not have header");
                                        return Ok(());
                                    }
                                }
                            }
                            if updated {
                                print_stats(&delays);
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

fn print_stats(delays: &DelayWindow) {
    let Some(stats) = delays.stats() else {
        return;
    };

    println!("average delay: {:.3}", stats.average_seconds);
    println!(
        "\tmin: {:.3}s max: {:.3}s std dev: {:.5}s window: {}",
        stats.min_seconds,
        stats.max_seconds,
        stats.std_dev_seconds,
        stats.window,
    );
}

fn resolve_message_type(topic_name: &str) -> anyhow::Result<String> {
    match send_request(CommandRequest::TopicType(TopicTypeRequest {
        topic_name: topic_name.to_string(),
    }))? {
        CommandResponse::TopicType(response) => match response.type_names.len() {
            0 => bail!("topic delay could not determine the message type for {}", topic_name),
            1 => Ok(response.type_names.into_iter().next().unwrap()),
            _ => bail!("topic delay found multiple types for {}; pass a single message type is not supported yet", topic_name),
        },
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic type request"),
    }
}

fn helper_script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("cli")
        .join("topic")
        .join("delay_helper.py")
}

struct PythonDelayExtractor {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl PythonDelayExtractor {
    fn spawn() -> anyhow::Result<Self> {
        let mut child = Command::new("python3")
            .arg(helper_script_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn topic delay python helper")?;
        let stdin = child.stdin.take().context("take topic delay helper stdin")?;
        let stdout = child.stdout.take().context("take topic delay helper stdout")?;
        Ok(Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    fn extract_delay_seconds(
        &mut self,
        type_name: &str,
        message: &TopicDelayMessage,
    ) -> anyhow::Result<DelayOutcome> {
        self.send(&DelayHelperRequest::Extract {
            type_name: type_name.to_string(),
            payload_base64: message.payload_base64.clone(),
            received_at_nanos: message.received_at_nanos,
        })?;
        match self.recv()? {
            DelayHelperResponse::Delay { delay_seconds } => Ok(DelayOutcome::Delay(delay_seconds)),
            DelayHelperResponse::MissingHeader => Ok(DelayOutcome::MissingHeader),
            DelayHelperResponse::Err { error } => bail!(error),
        }
    }

    fn send(&mut self, request: &DelayHelperRequest) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.stdin, request).context("serialize helper request")?;
        self.stdin.write_all(b"\n").context("write helper request newline")?;
        self.stdin.flush().context("flush helper request")
    }

    fn recv(&mut self) -> anyhow::Result<DelayHelperResponse> {
        let mut line = String::new();
        self.stdout.read_line(&mut line).context("read helper response")?;
        if line.trim().is_empty() {
            bail!("topic delay helper exited unexpectedly");
        }
        serde_json::from_str(line.trim_end()).context("parse helper response")
    }
}

enum DelayOutcome {
    Delay(f64),
    MissingHeader,
}

struct DelayWindow {
    values: VecDeque<f64>,
    capacity: usize,
}

struct DelayStats {
    average_seconds: f64,
    min_seconds: f64,
    max_seconds: f64,
    std_dev_seconds: f64,
    window: usize,
}

impl DelayWindow {
    fn new(capacity: usize) -> Self {
        Self {
            values: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
        }
    }

    fn push(&mut self, value: f64) {
        self.values.push_back(value);
        while self.values.len() > self.capacity {
            self.values.pop_front();
        }
    }

    fn stats(&self) -> Option<DelayStats> {
        if self.values.is_empty() {
            return None;
        }

        let window = self.values.len();
        let sum = self.values.iter().sum::<f64>();
        let average_seconds = sum / window as f64;
        let variance = self
            .values
            .iter()
            .map(|value| {
                let diff = *value - average_seconds;
                diff * diff
            })
            .sum::<f64>()
            / window as f64;
        let min_seconds = self.values.iter().copied().fold(f64::INFINITY, f64::min);
        let max_seconds = self.values.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        Some(DelayStats {
            average_seconds,
            min_seconds,
            max_seconds,
            std_dev_seconds: variance.sqrt(),
            window,
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DelayHelperRequest {
    Extract {
        type_name: String,
        payload_base64: String,
        received_at_nanos: u64,
    },
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum DelayHelperResponse {
    Delay { delay_seconds: f64 },
    MissingHeader,
    Err { error: String },
}
