use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

use anyhow::Context;

use crate::command::{
    protocol::{CommandRequest, CommandResponse},
    server::DEFAULT_COMMAND_SOCKET_PATH,
};

pub fn send_request(request: CommandRequest) -> anyhow::Result<CommandResponse> {
    let mut stream = UnixStream::connect(DEFAULT_COMMAND_SOCKET_PATH)
        .with_context(|| format!("connect command socket at {DEFAULT_COMMAND_SOCKET_PATH}"))?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).context("set read timeout")?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT)).context("set write timeout")?;
    serde_json::to_writer(&mut stream, &request).context("serialize request")?;
    stream.write_all(b"\n").context("write request newline")?;
    stream.flush().context("flush request")?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).context("read response")?;
    serde_json::from_str(line.trim_end()).context("parse response")
}

pub fn info_log(target: &str, message: impl AsRef<str>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    println!(
        "[INFO] [{}.{:09}] [{}]: {}",
        now.as_secs(),
        now.subsec_nanos(),
        target,
        message.as_ref()
    );
}
