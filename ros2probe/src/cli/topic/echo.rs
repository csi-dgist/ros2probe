use std::{
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use anyhow::{Context, bail};
use clap::Args;
use serde::{Deserialize, Serialize};

use super::super::bag::util::send_request;
use crate::command::protocol::{
    CommandRequest, CommandResponse, TopicEchoStartRequest, TopicEchoStopRequest, TopicTypeRequest,
};

const ECHO_HELPER_SOURCE: &str = include_str!("echo_helper.py");

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
        CommandResponse::TopicEchoStart(response) => {
            let stream_path = response
                .stream_path
                .context("runtime did not provide an echo stream path")?;
            let result = if args.raw {
                wait_for_raw_messages(&stream_path, args.once, args.no_lost_messages)
            } else {
                let default_type = resolved_type
                    .as_deref()
                    .context("topic echo could not determine the message type; pass [message_type] explicitly")?;
                wait_for_decoded_messages(&stream_path, &args, default_type)
            };
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

fn wait_for_raw_messages(
    stream_path: &str,
    once: bool,
    no_lost_messages: bool,
) -> anyhow::Result<()> {
    let stream = UnixStream::connect(stream_path)
        .with_context(|| format!("connect topic echo stream at {stream_path}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .context("set topic echo stream read timeout")?;
    let runtime = tokio::runtime::Runtime::new().context("create ctrl-c runtime")?;
    runtime.block_on(async {
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        loop {
            let read_task = tokio::task::spawn_blocking({
                let mut stream = stream.try_clone().context("clone topic echo stream")?;
                move || read_echo_frame(&mut stream)
            });
            tokio::pin!(read_task);
            tokio::select! {
                result = &mut ctrl_c => {
                    result.context("wait for ctrl-c")?;
                    return Ok(());
                }
                read_result = &mut read_task => {
                    match read_result.context("join topic echo stream reader")? {
                        Ok(frame) => {
                            print_raw_frame(&frame, no_lost_messages)?;
                            if once {
                                return Ok(());
                            }
                        }
                        Err(err) if is_timed_out(&err) => {}
                        Err(err) => return Err(err),
                    }
                }
            }
        }
    })
}

fn wait_for_decoded_messages(
    stream_path: &str,
    args: &TopicEchoCommand,
    default_type: &str,
) -> anyhow::Result<()> {
    let decoder = PythonEchoDecoder::spawn(
        stream_path.to_string(),
        default_type.to_string(),
        args.field.clone(),
        args.truncate_length,
        args.no_arr,
        args.no_str,
        args.csv,
    )?;
    let (response_tx, response_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut decoder = decoder;
        loop {
            match decoder.recv() {
                Ok(response) => {
                    if response_tx.send(Ok(response)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = response_tx.send(Err(err));
                    break;
                }
            }
        }
    });

    let response_rx = Arc::new(Mutex::new(response_rx));
    let runtime = tokio::runtime::Runtime::new().context("create ctrl-c runtime")?;
    runtime.block_on(async {
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        let mut printed = 0usize;
        loop {
            let recv_task = tokio::task::spawn_blocking({
                let response_rx = Arc::clone(&response_rx);
                move || {
                    response_rx
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .recv_timeout(Duration::from_millis(100))
                }
            });
            tokio::pin!(recv_task);
            tokio::select! {
                result = &mut ctrl_c => {
                    result.context("wait for ctrl-c")?;
                    return Ok(());
                }
                recv_result = &mut recv_task => {
                    match recv_result.context("join topic echo helper reader")? {
                        Ok(Ok(HelperResponse::Ok { rendered, lost_before })) => {
                            print_decoded_message(&rendered, lost_before, args.csv, args.no_lost_messages)?;
                            printed += 1;
                            if args.once && printed >= 1 {
                                return Ok(());
                            }
                        }
                        Ok(Ok(HelperResponse::Err { error })) => bail!(error),
                        Ok(Err(err)) => return Err(err),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
            }
        }
    })
}

fn print_raw_frame(frame: &EchoFrame, no_lost_messages: bool) -> anyhow::Result<()> {
    if !no_lost_messages && frame.lost_before > 0 {
        println!("{} message(s) lost", frame.lost_before);
    }
    println!("{}", hex_bytes(&frame.payload));
    Ok(())
}

fn print_decoded_message(
    rendered: &str,
    lost_before: usize,
    csv: bool,
    no_lost_messages: bool,
) -> anyhow::Result<()> {
    if !no_lost_messages && lost_before > 0 {
        println!("{} message(s) lost", lost_before);
    }
    println!("{rendered}");
    if !csv {
        println!("---");
    }
    io::stdout().flush().context("flush topic echo output")
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
            _ => bail!(
                "topic echo found multiple types for {}; pass [message_type] explicitly",
                topic_name
            ),
        },
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic type request"),
    }
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
        stream_path: String,
        default_type: String,
        field: Option<String>,
        truncate_length: usize,
        no_arr: bool,
        no_str: bool,
        csv: bool,
    ) -> anyhow::Result<Self> {
        let mut child = Command::new("python3")
            .arg("-u")
            .arg("-c")
            .arg(ECHO_HELPER_SOURCE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn topic echo python helper")?;
        let stdin = child.stdin.take().context("take topic echo helper stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("take topic echo helper stdout")?;
        let mut decoder = Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        decoder.send(&HelperRequest::Init {
            stream_path,
            default_type,
            field,
            truncate_length,
            no_arr,
            no_str,
            csv,
        })?;
        let response = decoder.recv()?;
        match response {
            HelperResponse::Ok { rendered, .. } => {
                let _ = rendered;
                Ok(decoder)
            }
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
        stream_path: String,
        default_type: String,
        field: Option<String>,
        truncate_length: usize,
        no_arr: bool,
        no_str: bool,
        csv: bool,
    },
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum HelperResponse {
    Ok {
        rendered: String,
        #[serde(default)]
        lost_before: usize,
    },
    Err {
        error: String,
    },
}

struct EchoFrame {
    lost_before: usize,
    payload: Vec<u8>,
}

fn read_echo_frame(stream: &mut UnixStream) -> anyhow::Result<EchoFrame> {
    let mut header = [0u8; 19];
    read_exact_or_timeout(stream, &mut header)?;
    if &header[0..4] != b"RPE1" {
        bail!("invalid topic echo stream frame");
    }
    if header[4] != 1 {
        bail!("unsupported topic echo stream version {}", header[4]);
    }
    let type_len = u16::from_le_bytes([header[5], header[6]]) as usize;
    let lost_before = u64::from_le_bytes(
        header[7..15]
            .try_into()
            .context("parse echo frame lost count")?,
    );
    let payload_len = u32::from_le_bytes(
        header[15..19]
            .try_into()
            .context("parse echo frame payload length")?,
    ) as usize;

    let mut discard_type = vec![0u8; type_len];
    read_exact_or_timeout(stream, &mut discard_type)?;
    let mut payload = vec![0u8; payload_len];
    read_exact_or_timeout(stream, &mut payload)?;
    Ok(EchoFrame {
        lost_before: usize::try_from(lost_before).unwrap_or(usize::MAX),
        payload,
    })
}

fn read_exact_or_timeout(stream: &mut UnixStream, buf: &mut [u8]) -> anyhow::Result<()> {
    match stream.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(err) if is_io_timed_out(&err) => Err(err).context("topic echo stream read timed out"),
        Err(err) => Err(err).context("read topic echo stream"),
    }
}

fn is_timed_out(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(is_io_timed_out)
    })
}

fn is_io_timed_out(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::TimedOut
}
