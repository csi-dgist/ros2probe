//! Recorder actor: owns the MCAP `Recorder` on its own thread so MCAP writes
//! (which can fsync and compress) never stall the main runtime task.
//!
//! Main runtime holds a `RecorderHandle` and publishes:
//! - **Data** via `try_record(...)` into a byte-bounded cache.
//! - **Commands** (Start / Stop / Shutdown) via a command channel. Stop drains
//!   all cached data before finalizing the MCAP.

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::Context;

use crate::{
    recorder::{RecordMessage, Recorder},
    runtime::CompressionConfig,
};

const RECORDER_CACHE_MAX_BYTES: usize = 256 * 1024 * 1024;
const RECORDER_BATCH_MAX_MESSAGES: usize = 1024;
const RECORDER_BATCH_MAX_BYTES: usize = 16 * 1024 * 1024;
const RECORDER_IDLE_WAIT: Duration = Duration::from_millis(1);

enum RecorderCommand {
    EnsureChannel {
        topic_name: String,
        schema_name: String,
        qos_profile: String,
        reply: mpsc::Sender<Result<u16, String>>,
    },
    Start {
        output: PathBuf,
        compression: CompressionConfig,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Stop {
        reply: mpsc::Sender<Result<Option<PathBuf>, String>>,
    },
    Shutdown,
}

struct CachedRecordMessage {
    message: RecordMessage,
    bytes: usize,
}

#[derive(Default)]
struct RecordCache {
    messages: VecDeque<CachedRecordMessage>,
    bytes: usize,
}

pub(crate) struct RecorderHandle {
    tx: mpsc::Sender<RecorderCommand>,
    cache: Arc<Mutex<RecordCache>>,
    dropped_count: Arc<AtomicUsize>,
    dropped_by_topic: Arc<Mutex<BTreeMap<String, usize>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RecorderHandle {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let cache = Arc::new(Mutex::new(RecordCache::default()));
        let dropped_count = Arc::new(AtomicUsize::new(0));
        let dropped_by_topic = Arc::new(Mutex::new(BTreeMap::new()));
        let actor_cache = Arc::clone(&cache);
        let thread = thread::Builder::new()
            .name(String::from("ros2probe-recorder"))
            .spawn(move || actor_loop(rx, actor_cache))
            .expect("spawn recorder actor thread");
        Self {
            tx,
            cache,
            dropped_count,
            dropped_by_topic,
            thread: Some(thread),
        }
    }

    /// Open a new MCAP at `output`. Blocks until the actor has opened the
    /// file so the caller can surface creation errors synchronously.
    pub(crate) fn start(
        &self,
        output: PathBuf,
        compression: CompressionConfig,
    ) -> anyhow::Result<()> {
        self.reset_drop_counts();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(RecorderCommand::Start {
                output,
                compression,
                reply: reply_tx,
            })
            .context("recorder actor disconnected")?;
        let reply = reply_rx.recv().context("recorder actor dropped reply")?;
        reply.map_err(|err| anyhow::anyhow!(err))
    }

    pub(crate) fn ensure_channel(
        &self,
        topic_name: &str,
        schema_name: &str,
        qos_profile: &str,
    ) -> anyhow::Result<u16> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(RecorderCommand::EnsureChannel {
                topic_name: topic_name.to_string(),
                schema_name: schema_name.to_string(),
                qos_profile: qos_profile.to_string(),
                reply: reply_tx,
            })
            .context("recorder actor disconnected")?;
        reply_rx
            .recv()
            .context("recorder actor dropped reply")?
            .map_err(|err| anyhow::anyhow!(err))
    }

    /// Finalize the current recording. Any data messages queued *before* this
    /// call are guaranteed to be written first (single channel, FIFO order).
    pub(crate) fn stop(&self) -> anyhow::Result<(Option<PathBuf>, Vec<(String, usize)>)> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(RecorderCommand::Stop { reply: reply_tx })
            .context("recorder actor disconnected")?;
        let output = reply_rx
            .recv()
            .context("recorder actor dropped reply")?
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok((output, self.drop_counts()))
    }

    /// Non-blocking data send. Drops the message if the actor can't keep up
    /// (byte-bounded cache full). Returns `false` on drop.
    pub(crate) fn try_record(&self, message: RecordMessage, topic_name: &str) -> bool {
        let bytes = message.payload.len();
        let mut cache = self.cache.lock().unwrap_or_else(|err| err.into_inner());
        if bytes > RECORDER_CACHE_MAX_BYTES
            || cache
                .bytes
                .checked_add(bytes)
                .is_none_or(|next| next > RECORDER_CACHE_MAX_BYTES)
        {
            drop(cache);
            self.record_drop(topic_name);
            return false;
        }

        cache.bytes += bytes;
        cache.messages.push_back(CachedRecordMessage { message, bytes });
        true
    }

    /// Total messages dropped since startup because the actor couldn't keep
    /// up with producers. Monotonic counter. Exposed for future surfacing in
    /// `BagStatus`; currently unread.
    #[allow(dead_code)]
    pub(crate) fn dropped_count(&self) -> usize {
        self.dropped_count.load(Ordering::Relaxed)
    }

    fn record_drop(&self, topic_name: &str) {
        self.dropped_count.fetch_add(1, Ordering::Relaxed);
        let mut dropped_by_topic = self
            .dropped_by_topic
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        *dropped_by_topic.entry(topic_name.to_string()).or_insert(0) += 1;
    }

    fn reset_drop_counts(&self) {
        self.dropped_count.store(0, Ordering::Relaxed);
        self.dropped_by_topic
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
        let mut cache = self.cache.lock().unwrap_or_else(|err| err.into_inner());
        cache.messages.clear();
        cache.bytes = 0;
    }

    fn drop_counts(&self) -> Vec<(String, usize)> {
        self.dropped_by_topic
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .map(|(topic_name, count)| (topic_name.clone(), *count))
            .collect()
    }
}

impl Drop for RecorderHandle {
    fn drop(&mut self) {
        // Graceful shutdown: actor drains everything already in the queue,
        // finalizes any open recording, then exits.
        let _ = self.tx.send(RecorderCommand::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn actor_loop(rx: mpsc::Receiver<RecorderCommand>, cache: Arc<Mutex<RecordCache>>) {
    let mut active: Option<Recorder> = None;
    loop {
        while let Ok(command) = rx.try_recv() {
            if handle_command(command, &mut active, &cache) {
                return;
            }
        }

        let batch = take_batch(&cache);
        if !batch.is_empty() {
            write_batch(active.as_mut(), batch);
            continue;
        }

        match rx.recv_timeout(RECORDER_IDLE_WAIT) {
            Ok(command) => {
                if handle_command(command, &mut active, &cache) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                drain_cache(&mut active, &cache);
                if let Some(recorder) = active.take() {
                    let _ = recorder.finish();
                }
                return;
            }
        }
    }
}

fn handle_command(
    command: RecorderCommand,
    active: &mut Option<Recorder>,
    cache: &Arc<Mutex<RecordCache>>,
) -> bool {
    match command {
        RecorderCommand::EnsureChannel {
            topic_name,
            schema_name,
            qos_profile,
            reply,
        } => {
            let result = match active.as_mut() {
                Some(recorder) => recorder
                    .ensure_channel(&topic_name, &schema_name, &qos_profile)
                    .map_err(|err| format!("{err:#}")),
                None => Err(String::from("recorder actor has no active session")),
            };
            let _ = reply.send(result);
            false
        }
        RecorderCommand::Start {
            output,
            compression,
            reply,
        } => {
            clear_cache(cache);
            let result = if active.is_some() {
                Err(String::from("recorder actor already has an active session"))
            } else {
                match Recorder::create(&output, compression) {
                    Ok(recorder) => {
                        *active = Some(recorder);
                        Ok(())
                    }
                    Err(err) => Err(format!("{err:#}")),
                }
            };
            let _ = reply.send(result);
            false
        }
        RecorderCommand::Stop { reply } => {
            drain_cache(active, cache);
            let result = match active.take() {
                Some(recorder) => match recorder.finish() {
                    Ok(path) => Ok(Some(path)),
                    Err(err) => Err(format!("{err:#}")),
                },
                None => Ok(None),
            };
            let _ = reply.send(result);
            false
        }
        RecorderCommand::Shutdown => {
            drain_cache(active, cache);
            if let Some(recorder) = active.take() {
                let _ = recorder.finish();
            }
            true
        }
    }
}

fn drain_cache(active: &mut Option<Recorder>, cache: &Arc<Mutex<RecordCache>>) {
    loop {
        let batch = take_batch(cache);
        if batch.is_empty() {
            return;
        }
        write_batch(active.as_mut(), batch);
    }
}

fn write_batch(mut recorder: Option<&mut Recorder>, batch: Vec<RecordMessage>) {
    let Some(recorder) = recorder.as_deref_mut() else {
        return;
    };

    for message in batch {
        if let Err(err) = recorder.write_record_message(&message) {
            log::warn!("MCAP write failed: {err:#}");
        }
    }
}

fn take_batch(cache: &Arc<Mutex<RecordCache>>) -> Vec<RecordMessage> {
    let mut cache = cache.lock().unwrap_or_else(|err| err.into_inner());
    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;

    while batch.len() < RECORDER_BATCH_MAX_MESSAGES {
        let Some(entry) = cache.messages.pop_front() else {
            break;
        };
        cache.bytes = cache.bytes.saturating_sub(entry.bytes);
        batch_bytes += entry.bytes;
        batch.push(entry.message);
        if batch_bytes >= RECORDER_BATCH_MAX_BYTES {
            break;
        }
    }

    batch
}

fn clear_cache(cache: &Arc<Mutex<RecordCache>>) {
    let mut cache = cache.lock().unwrap_or_else(|err| err.into_inner());
    cache.messages.clear();
    cache.bytes = 0;
}
