use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, bail};
use base64::Engine;
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::command::{
    protocol::{
        CommandRequest, CommandResponse, TopicEchoMessage, TopicEchoStartRequest,
        TopicEchoStatusRequest, TopicEchoStopRequest, TopicTypeRequest,
    },
};

use super::super::bag::util::send_request;

#[derive(Debug, Args)]
pub struct TopicEchoCommand {
    /// Name of the ROS topic to observe
    pub topic_name: String,

    /// Type of the ROS message
    pub message_type: Option<String>,

    /// Echo the raw binary representation
    #[arg(long = "raw")]
    pub raw: bool,

    /// Echo a selected field of a message, e.g. pose.pose.position
    #[arg(long = "field")]
    pub field: Option<String>,

    /// Print the first message received and then exit
    #[arg(long = "once")]
    pub once: bool,

    /// Don't print array fields of messages
    #[arg(long = "no-arr")]
    pub no_arr: bool,

    /// Don't print string fields of messages
    #[arg(long = "no-str")]
    pub no_str: bool,

    /// The length to truncate arrays, bytes, and strings to
    #[arg(long = "truncate-length", short = 'l', default_value_t = 128)]
    pub truncate_length: usize,

    /// Output all recursive fields separated by commas (e.g. for plotting)
    #[arg(long = "csv")]
    pub csv: bool,

    /// Don't report when a message is lost
    #[arg(long = "no-lost-messages")]
    pub no_lost_messages: bool,
}

pub fn run(args: TopicEchoCommand) -> anyhow::Result<()> {
    let resolved_type = if args.raw {
        args.message_type.clone()
    } else {
        resolve_message_type(&args.topic_name, args.message_type.clone())?
    };

    let response = send_request(CommandRequest::TopicEchoStart(TopicEchoStartRequest {
        topic_name: args.topic_name.clone(),
    }))?;

    match response {
        CommandResponse::TopicEchoStart(_) => {
            let mut decoder = if args.raw {
                None
            } else {
                Some(PythonEchoDecoder::spawn(
                    args.field.clone(),
                    args.truncate_length,
                    args.no_arr,
                    args.no_str,
                    args.csv,
                )?)
            };
            let result = wait_for_messages(&args, resolved_type.as_deref(), decoder.as_mut());
            let stop_result = send_request(CommandRequest::TopicEchoStop(TopicEchoStopRequest));
            match (result, stop_result) {
                (Ok(()), Ok(CommandResponse::TopicEchoStop(_))) => Ok(()),
                (Ok(()), Ok(CommandResponse::Error(error))) => bail!(error.message),
                (Ok(()), Ok(_)) => bail!("unexpected response for topic echo stop request"),
                (Ok(()), Err(err)) => Err(err),
                (Err(err), _) => Err(err),
            }
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic echo start request"),
    }
}

fn wait_for_messages(
    args: &TopicEchoCommand,
    requested_type: Option<&str>,
    mut decoder: Option<&mut PythonEchoDecoder>,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create ctrl-c runtime")?;
    runtime.block_on(async {
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        loop {
            let status_task = tokio::task::spawn_blocking(|| {
                send_request(CommandRequest::TopicEchoStatus(TopicEchoStatusRequest))
            });
            tokio::pin!(status_task);
            tokio::select! {
                result = &mut ctrl_c => {
                    result.context("wait for ctrl-c")?;
                    return Ok(());
                }
                status_result = &mut status_task => {
                    match status_result.context("join topic echo status task")?? {
                        CommandResponse::TopicEchoStatus(response) => {
                            for message in response.messages {
                                let effective_type = requested_type
                                    .map(str::to_string)
                                    .or(message.type_name.clone());
                                if !args.raw && effective_type.is_none() {
                                    bail!("topic echo could not determine the message type; pass [message_type] explicitly");
                                }
                                print_message(
                                    &message,
                                    effective_type.as_deref(),
                                    args.raw,
                                    args.csv,
                                    args.no_lost_messages,
                                    decoder.as_deref_mut(),
                                )?;
                                if args.once {
                                    return Ok(());
                                }
                            }
                            if !response.active {
                                return Ok(());
                            }
                        }
                        CommandResponse::Error(error) => bail!(error.message),
                        _ => bail!("unexpected response for topic echo status request"),
                    }
                }
            }
        }
    })
}

fn print_message(
    message: &TopicEchoMessage,
    type_name: Option<&str>,
    raw: bool,
    csv: bool,
    no_lost_messages: bool,
    decoder: Option<&mut PythonEchoDecoder>,
) -> anyhow::Result<()> {
    if !no_lost_messages && message.lost_before > 0 {
        println!("{} message(s) lost", message.lost_before);
    }

    if raw {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&message.payload_base64)
            .context("decode echoed payload from base64")?;
        println!("{}", hex_bytes(&bytes));
        return Ok(());
    }

    let decoder = decoder.context("topic echo decoder was not initialized")?;
    let type_name = type_name.context("missing message type for decoded echo")?;
    let rendered = decoder.decode(type_name, &message.payload_base64)?;
    println!("{rendered}");
    if !csv {
        println!("---");
    }
    io::stdout().flush().context("flush topic echo output")?;
    Ok(())
}

fn resolve_message_type(
    topic_name: &str,
    explicit_type: Option<String>,
) -> anyhow::Result<Option<String>> {
    if explicit_type.is_some() {
        return Ok(explicit_type);
    }

    match send_request(CommandRequest::TopicType(TopicTypeRequest {
        topic_name: topic_name.to_string(),
    }))? {
        CommandResponse::TopicType(response) => match response.type_names.len() {
            0 => Ok(None),
            1 => Ok(response.type_names.into_iter().next()),
            _ => bail!("topic echo found multiple types for {}; pass [message_type] explicitly", topic_name),
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
        .join("echo_helper.py")
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

struct PythonEchoDecoder {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl PythonEchoDecoder {
    fn spawn(
        field: Option<String>,
        truncate_length: usize,
        no_arr: bool,
        no_str: bool,
        csv: bool,
    ) -> anyhow::Result<Self> {
        let mut child = Command::new("python3")
            .arg(helper_script_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn topic echo python helper")?;
        let stdin = child.stdin.take().context("take topic echo helper stdin")?;
        let stdout = child.stdout.take().context("take topic echo helper stdout")?;
        let mut decoder = Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        decoder.send(&HelperRequest::Init {
            field,
            truncate_length,
            no_arr,
            no_str,
            csv,
        })?;
        let response = decoder.recv()?;
        match response {
            HelperResponse::Ok { rendered } => {
                let _ = rendered;
                Ok(decoder)
            }
            HelperResponse::Err { error } => bail!(error),
        }
    }

    fn decode(&mut self, type_name: &str, payload_base64: &str) -> anyhow::Result<String> {
        self.send(&HelperRequest::Decode {
            type_name: type_name.to_string(),
            payload_base64: payload_base64.to_string(),
        })?;
        match self.recv()? {
            HelperResponse::Ok { rendered } => Ok(rendered),
            HelperResponse::Err { error } => bail!(error),
        }
    }

    fn send(&mut self, request: &HelperRequest) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.stdin, request).context("serialize helper request")?;
        self.stdin
            .write_all(b"\n")
            .context("write helper request newline")?;
        self.stdin.flush().context("flush helper request")
    }

    fn recv(&mut self) -> anyhow::Result<HelperResponse> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .context("read helper response")?;
        if line.trim().is_empty() {
            bail!("topic echo helper exited unexpectedly");
        }
        serde_json::from_str(line.trim_end()).context("parse helper response")
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HelperRequest {
    Init {
        field: Option<String>,
        truncate_length: usize,
        no_arr: bool,
        no_str: bool,
        csv: bool,
    },
    Decode {
        type_name: String,
        payload_base64: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum HelperResponse {
    Ok { rendered: String },
    Err { error: String },
}
