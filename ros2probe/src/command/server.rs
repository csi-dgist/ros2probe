use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::fs::{FileTypeExt, PermissionsExt},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const BUSY_WRITE_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// Cap on concurrent client-handler threads. Beyond this, new connections are
/// handled inline on the accept thread so a flood of clients cannot spawn an
/// unbounded number of threads.
const MAX_CONCURRENT_CLIENTS: usize = 32;

use crate::command::{
    action, bag, discover, node,
    protocol::{CommandRequest, CommandResponse, ErrorResponse},
    service,
    state::SharedState,
    topic,
};
use crate::runtime::RuntimeCommand;

pub const DEFAULT_COMMAND_SOCKET_PATH: &str = "/tmp/ros2probe.sock";

pub fn default_socket_path() -> PathBuf {
    PathBuf::from(DEFAULT_COMMAND_SOCKET_PATH)
}

pub fn spawn(
    socket_path: impl AsRef<Path>,
    command_state: SharedState,
    runtime_command_tx: mpsc::Sender<RuntimeCommand>,
) -> std::io::Result<thread::JoinHandle<()>> {
    let socket_path = socket_path.as_ref().to_path_buf();
    prepare_socket_path(&socket_path)?;

    let listener = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666))?;
    let in_flight = Arc::new(AtomicUsize::new(0));
    Ok(thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    if let Err(e) = stream.set_read_timeout(Some(CLIENT_READ_TIMEOUT)) {
                        eprintln!("command socket set_read_timeout failed: {e}");
                    }
                    if let Err(e) = stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT)) {
                        eprintln!("command socket set_write_timeout failed: {e}");
                    }
                    let admitted = in_flight
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                            (count < MAX_CONCURRENT_CLIENTS).then_some(count + 1)
                        })
                        .is_ok();
                    if !admitted {
                        let mut stream = stream;
                        let _ = stream.set_write_timeout(Some(BUSY_WRITE_TIMEOUT));
                        let response = CommandResponse::Error(ErrorResponse {
                            message: String::from("command server busy; try again"),
                        });
                        let _ = write_response(&mut stream, &response);
                    } else {
                        let state = Arc::clone(&command_state);
                        let tx = runtime_command_tx.clone();
                        let counter = Arc::clone(&in_flight);
                        thread::spawn(move || {
                            handle_client(stream, &state, &tx);
                            counter.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                }
                Err(err) => eprintln!("command socket accept failed: {err}"),
            }
        }
    }))
}

fn prepare_socket_path(socket_path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "command socket path {} is not a socket",
                socket_path.display()
            ),
        ));
    }

    match UnixStream::connect(socket_path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another ros2probe runtime is already listening at {}",
                socket_path.display()
            ),
        )),
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(socket_path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

// Reads the shared CommandState via `(**command_state.load()).clone()`. Each
// request arm issues exactly one `load()` and drops the ArcSwap guard at the
// end of the expression, staying inside the cheap per-thread guard slot. If
// you add a second `load()` in the same arm, reuse the first guard (or switch
// to `load_full()` once) to avoid the fallback refcount path.
fn handle_client(
    stream: UnixStream,
    command_state: &SharedState,
    runtime_command_tx: &mpsc::Sender<RuntimeCommand>,
) {
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(err) => {
            eprintln!("command socket clone failed: {err}");
            return;
        }
    };
    let mut reader = BufReader::new(stream);
    let line = match read_request_line(&mut reader) {
        Ok(line) => line,
        Err(err) => {
            let _ = write_response(
                &mut writer,
                &CommandResponse::Error(ErrorResponse {
                    message: format!("read request failed: {err}"),
                }),
            );
            return;
        }
    };

    let request = match serde_json::from_str::<CommandRequest>(line.trim_end()) {
        Ok(request) => request,
        Err(err) => {
            let _ = write_response(
                &mut writer,
                &CommandResponse::Error(ErrorResponse {
                    message: format!("invalid request: {err}"),
                }),
            );
            return;
        }
    };

    let response =
        match request {
            CommandRequest::ActionInfo(request) => CommandResponse::ActionInfo(
                action::info::build_response(request, (**command_state.load()).clone()),
            ),
            CommandRequest::ActionList(request) => CommandResponse::ActionList(
                action::list::build_response(request, (**command_state.load()).clone()),
            ),
            CommandRequest::BagRecord(request) => {
                match bag::record::build_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::BagRecord(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::BagStatus(request) => {
                match bag::status::build_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::BagStatus(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::BagSetPaused(request) => {
                match bag::set_paused::build_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::BagSetPaused(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::BagStop(request) => {
                match bag::stop::build_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::BagStop(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::NodeInfo(request) => CommandResponse::NodeInfo(
                node::info::build_response(request, (**command_state.load()).clone()),
            ),
            CommandRequest::NodeList(request) => CommandResponse::NodeList(
                node::list::build_response(request, (**command_state.load()).clone()),
            ),
            CommandRequest::TopicBwStart(request) => {
                match topic::bw::build_start_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicBwStart(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicBwStatus(request) => {
                match topic::bw::build_status_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicBwStatus(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicBwStop(request) => {
                match topic::bw::build_stop_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicBwStop(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicDelayStart(request) => {
                match topic::delay::build_start_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicDelayStart(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicDelayStatus(request) => {
                match topic::delay::build_status_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicDelayStatus(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicDelayStop(request) => {
                match topic::delay::build_stop_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicDelayStop(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicEchoStart(request) => {
                match topic::echo::build_start_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicEchoStart(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicEchoStatus(request) => {
                match topic::echo::build_status_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicEchoStatus(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicEchoStop(request) => {
                match topic::echo::build_stop_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicEchoStop(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicHzBwStatus(_request) => {
                match topic::hz::build_hz_bw_status_response(runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicHzBwStatus(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicHzStart(request) => {
                match topic::hz::build_start_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicHzStart(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicHzStatus(request) => {
                match topic::hz::build_status_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicHzStatus(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicHzStop(request) => {
                match topic::hz::build_stop_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::TopicHzStop(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::ServiceList(request) => CommandResponse::ServiceList(
                service::list::build_response(request, (**command_state.load()).clone()),
            ),
            CommandRequest::ServiceType(request) => CommandResponse::ServiceType(
                service::r#type::build_response(request, (**command_state.load()).clone()),
            ),
            CommandRequest::Discover(request) => {
                match discover::run::build_response(request, runtime_command_tx) {
                    Ok(response) => CommandResponse::Discover(response),
                    Err(err) => CommandResponse::Error(ErrorResponse {
                        message: err.to_string(),
                    }),
                }
            }
            CommandRequest::TopicInfo(request) => CommandResponse::TopicInfo(
                topic::info::build_response(request, (**command_state.load()).clone()),
            ),
            CommandRequest::TopicList(request) => CommandResponse::TopicList(
                topic::list::build_response(request, (**command_state.load()).clone()),
            ),
            CommandRequest::TopicType(request) => CommandResponse::TopicType(
                topic::r#type::build_response(request, (**command_state.load()).clone()),
            ),
            CommandRequest::TopicGraph(request) => CommandResponse::TopicGraph(
                topic::graph::build_response(request, (**command_state.load()).clone()),
            ),
        };
    let _ = write_response(&mut writer, &response);
}

fn read_request_line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    reader
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_line(&mut line)?;
    if line.len() > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("request exceeds {MAX_REQUEST_BYTES} bytes"),
        ));
    }
    Ok(line)
}

fn write_response(writer: &mut UnixStream, response: &CommandResponse) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, response)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn request_reader_rejects_oversized_line() {
        let mut request = vec![b'x'; MAX_REQUEST_BYTES + 1];
        request.push(b'\n');
        let mut reader = Cursor::new(request);

        let err = read_request_line(&mut reader).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn socket_path_preserves_active_server_and_removes_stale_socket() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = PathBuf::from(format!(
            "/tmp/ros2probe-server-test-{}-{unique}.sock",
            std::process::id()
        ));
        let listener = UnixListener::bind(&path).unwrap();

        let err = prepare_socket_path(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists());

        drop(listener);
        prepare_socket_path(&path).unwrap();
        assert!(!path.exists());
    }
}
