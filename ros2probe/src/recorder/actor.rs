//! Recorder actor: owns the MCAP `Recorder` on its own thread so MCAP writes
//! (which can fsync and compress) never stall the main runtime task.
//!
//! Main runtime holds a `RecorderHandle` and publishes:
//! - **Data** via `try_record(...)` (bounded channel, drops on overload).
//! - **Commands** (Start / Stop / Shutdown) via blocking `send`, which inherits
//!   the channel's FIFO ordering — any data queued before `Stop` is guaranteed
//!   to be written to the MCAP before the recording is finalized.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use anyhow::Context;

use crate::{
    protocols::RtpsDataMessage,
    recorder::{Recorder, RecorderTopicMetadata},
    runtime::CompressionConfig,
};

/// Channel capacity for actor messages. Chosen to absorb brief write stalls
/// (e.g. zstd compression flush) without OOM: a typical ROS topic at 100 Hz
/// fills the buffer in 10 s, by which time the MCAP writer has caught up or
/// drops will show up in `dropped_count()` and become a visible signal.
const RECORDER_CHANNEL_CAPACITY: usize = 1000;

enum RecorderEvent {
    Data {
        message: RtpsDataMessage,
        metadata: RecorderTopicMetadata,
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

pub(crate) struct RecorderHandle {
    tx: mpsc::SyncSender<RecorderEvent>,
    dropped_count: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RecorderHandle {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = mpsc::sync_channel(RECORDER_CHANNEL_CAPACITY);
        let dropped_count = Arc::new(AtomicUsize::new(0));
        let thread = thread::Builder::new()
            .name(String::from("ros2probe-recorder"))
            .spawn(move || actor_loop(rx))
            .expect("spawn recorder actor thread");
        Self {
            tx,
            dropped_count,
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
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(RecorderEvent::Start {
                output,
                compression,
                reply: reply_tx,
            })
            .context("recorder actor disconnected")?;
        let reply = reply_rx.recv().context("recorder actor dropped reply")?;
        reply.map_err(|err| anyhow::anyhow!(err))
    }

    /// Finalize the current recording. Any data messages queued *before* this
    /// call are guaranteed to be written first (single channel, FIFO order).
    pub(crate) fn stop(&self) -> anyhow::Result<Option<PathBuf>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(RecorderEvent::Stop { reply: reply_tx })
            .context("recorder actor disconnected")?;
        reply_rx
            .recv()
            .context("recorder actor dropped reply")?
            .map_err(|err| anyhow::anyhow!(err))
    }

    /// Non-blocking data send. Drops the message if the actor can't keep up
    /// (channel full). Returns `false` on drop or disconnect.
    pub(crate) fn try_record(
        &self,
        message: RtpsDataMessage,
        metadata: RecorderTopicMetadata,
    ) -> bool {
        match self
            .tx
            .try_send(RecorderEvent::Data { message, metadata })
        {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    }

    /// Total messages dropped since startup because the actor couldn't keep
    /// up with producers. Monotonic counter. Exposed for future surfacing in
    /// `BagStatus`; currently unread.
    #[allow(dead_code)]
    pub(crate) fn dropped_count(&self) -> usize {
        self.dropped_count.load(Ordering::Relaxed)
    }
}

impl Drop for RecorderHandle {
    fn drop(&mut self) {
        // Graceful shutdown: actor drains everything already in the queue,
        // finalizes any open recording, then exits.
        let _ = self.tx.send(RecorderEvent::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn actor_loop(rx: mpsc::Receiver<RecorderEvent>) {
    let mut active: Option<Recorder> = None;
    while let Ok(event) = rx.recv() {
        match event {
            RecorderEvent::Data { message, metadata } => {
                if let Some(recorder) = active.as_mut() {
                    if let Err(err) = recorder.write_rtps_data_message(&message, &metadata) {
                        log::warn!("MCAP write failed: {err:#}");
                    }
                }
                // Else: session not active (message arrived before Start or
                // after Stop). Silently discard.
            }
            RecorderEvent::Start {
                output,
                compression,
                reply,
            } => {
                let result = if active.is_some() {
                    Err(String::from("recorder actor already has an active session"))
                } else {
                    match Recorder::create(&output, compression) {
                        Ok(recorder) => {
                            active = Some(recorder);
                            Ok(())
                        }
                        Err(err) => Err(format!("{err:#}")),
                    }
                };
                let _ = reply.send(result);
            }
            RecorderEvent::Stop { reply } => {
                let result = match active.take() {
                    Some(recorder) => match recorder.finish() {
                        Ok(path) => Ok(Some(path)),
                        Err(err) => Err(format!("{err:#}")),
                    },
                    None => Ok(None),
                };
                let _ = reply.send(result);
            }
            RecorderEvent::Shutdown => {
                // Drain remaining data then finalize open recording.
                while let Ok(event) = rx.try_recv() {
                    if let RecorderEvent::Data { message, metadata } = event {
                        if let Some(recorder) = active.as_mut() {
                            let _ = recorder.write_rtps_data_message(&message, &metadata);
                        }
                    }
                    // Further commands after Shutdown are ignored.
                }
                if let Some(recorder) = active.take() {
                    let _ = recorder.finish();
                }
                return;
            }
        }
    }
    // Channel closed without explicit Shutdown (all senders dropped).
    if let Some(recorder) = active.take() {
        let _ = recorder.finish();
    }
}

