use std::io::Read;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Args;
use tokio::runtime::Runtime;

use crate::command::{
    protocol::{
        BagRecordRequest, BagSetPausedRequest, BagStopRequest, CommandRequest, CommandResponse,
        CompressionFormat,
    },
};

use super::util::{info_log, send_request};

#[derive(Debug, Args)]
pub struct BagRecordCommand {
    /// List of topics to record
    #[arg(value_name = "TOPIC", required_unless_present = "all")]
    pub topics: Vec<String>,

    /// Record all topics
    #[arg(short = 'a', long = "all", conflicts_with = "topics")]
    pub all: bool,

    /// Destination MCAP file to create
    #[arg(short = 'o', long = "output")]
    pub output: Option<String>,

    /// Compression format to apply to the bag file
    #[arg(long = "compression-format", value_enum)]
    pub compression_format: Option<BagCompressionFormat>,

    /// Only record topics discovered when recording starts
    #[arg(long = "no-discovery")]
    pub no_discovery: bool,

    /// Start the recorder in a paused state
    #[arg(long = "start-paused")]
    pub start_paused: bool,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum BagCompressionFormat {
    None,
    Zstd,
    Lz4,
}

pub fn run(args: BagRecordCommand) -> anyhow::Result<()> {
    if !args.all && args.topics.is_empty() {
        bail!("pass one or more topics, or use --all");
    }

    let response = send_request(CommandRequest::BagRecord(BagRecordRequest {
        all: args.all,
        topics: args.topics,
        output: args.output,
        compression_format: args.compression_format.map(Into::into),
        no_discovery: args.no_discovery,
        start_paused: args.start_paused,
    }))?;

    match response {
        CommandResponse::BagRecord(response) => {
            info_log("ros2probe_recorder", "Press SPACE for pausing/resuming");
            info_log("ros2probe_recorder", "Press CTRL+C to stop recording");
            info_log(
                "ros2probe_storage",
                format!("Opened bag '{}' for READ_WRITE.", response.output),
            );
            info_log("ros2probe_recorder", "Listening for topics...");
            info_log(
                "ros2probe_recorder",
                if response.paused {
                    "Waiting for recording: Press SPACE to start."
                } else {
                    "Recording..."
                },
            );
            wait_for_controls(response.paused)?;
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for bag record request"),
    }
}

impl From<BagCompressionFormat> for CompressionFormat {
    fn from(value: BagCompressionFormat) -> Self {
        match value {
            BagCompressionFormat::None => CompressionFormat::None,
            BagCompressionFormat::Zstd => CompressionFormat::Zstd,
            BagCompressionFormat::Lz4 => CompressionFormat::Lz4,
        }
    }
}

fn wait_for_controls(mut paused: bool) -> anyhow::Result<()> {
    let _stdin_raw_mode = LocalStdinRawMode::enable()?;
    let input_rx = spawn_input_reader();
    let runtime = Runtime::new().context("create ctrl-c runtime")?;
    runtime.block_on(async {
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        loop {
            if let Ok(byte) = input_rx.try_recv() {
                if byte == b' ' {
                    match send_request(CommandRequest::BagSetPaused(BagSetPausedRequest {
                        paused: !paused,
                    }))? {
                        CommandResponse::BagSetPaused(response) => {
                            if !response.active {
                                bail!("recording is not active");
                            }
                            paused = response.paused;
                            info_log(
                                "ros2probe_recorder",
                                if paused {
                                    "Pausing recording."
                                } else {
                                    "Resuming recording."
                                },
                            );
                        }
                        CommandResponse::Error(error) => bail!(error.message),
                        _ => bail!("unexpected response for bag pause request"),
                    }
                }
            }

            tokio::select! {
                result = &mut ctrl_c => {
                    result.context("wait for ctrl-c")?;
                    info_log("ros2probe_recorder", "Stopping recording...");
                    match send_request(CommandRequest::BagStop(BagStopRequest))? {
                        CommandResponse::BagStop(response) => {
                            if response.stopped {
                                if let Some(output) = response.output {
                                    info_log(
                                        "ros2probe_storage",
                                        format!("Closed bag '{}'.", output),
                                    );
                                } else {
                                    info_log("ros2probe_storage", "Closed bag.");
                                }
                                info_log("ros2probe_recorder", "Recording stopped");
                            } else {
                                info_log("ros2probe_recorder", "Recording already stopped");
                            }
                            return Ok(());
                        }
                        CommandResponse::Error(error) => bail!(error.message),
                        _ => bail!("unexpected response for bag stop request"),
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    })
}

fn spawn_input_reader() -> mpsc::Receiver<u8> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let mut buf = [0u8; 1];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let _ = tx.send(buf[0]);
                }
                Err(_) => break,
            }
        }
    });
    rx
}

struct LocalStdinRawMode {
    original: Option<libc::termios>,
}

impl LocalStdinRawMode {
    fn enable() -> anyhow::Result<Self> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return Ok(Self { original: None });
        }

        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, termios.as_mut_ptr()) } != 0 {
            bail!("tcgetattr failed for stdin");
        }
        let original = unsafe { termios.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            bail!("tcsetattr failed for stdin");
        }

        Ok(Self {
            original: Some(original),
        })
    }
}

impl Drop for LocalStdinRawMode {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original) };
        }
    }
}
