//! The LAST LINE column's background reader (I4, I7): a thread of its own
//! that runs [`read_tails`] over a [`LogReader`], so no frame does IO. A
//! frame hands it the visible rows' requests and picks up what it found on a
//! later frame; one batch is out at a time.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use super::list::{LogReader, TailRequest, TailUpdate, read_tails};

/// How often a frame may hand the reader a new batch: a running row's log is
/// stat'ed at most this often.
const MIN_INTERVAL_MS: u64 = 250;

/// The frame's end of the reader thread. Dropping it ends the thread once
/// its batch in hand is read.
#[derive(Debug)]
pub struct TailReader {
    requests: Sender<Vec<TailRequest>>,
    updates: Receiver<Vec<TailUpdate>>,
    in_flight: bool,
    last_sent_ms: Option<u64>,
}

impl TailReader {
    /// Start the thread. `None` when it cannot be spawned: the column then
    /// shows only wait and failure text.
    pub fn spawn(reader: Arc<dyn LogReader>) -> Option<Self> {
        let (requests, todo) = mpsc::channel::<Vec<TailRequest>>();
        let (found, updates) = mpsc::channel();
        std::thread::Builder::new()
            .name("heph-log-tails".into())
            .spawn(move || {
                while let Ok(batch) = todo.recv() {
                    if found.send(read_tails(&*reader, &batch)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| tracing::debug!(error = %e, "log tail reader unavailable"))
            .ok()?;
        Some(Self {
            requests,
            updates,
            in_flight: false,
            last_sent_ms: None,
        })
    }

    /// What the reader found since the last call, if its batch is done.
    pub fn poll(&mut self) -> Option<Vec<TailUpdate>> {
        match self.updates.try_recv() {
            Ok(updates) => {
                self.in_flight = false;
                Some(updates)
            }
            Err(TryRecvError::Empty) => None,
            // The thread is gone (it panicked): nothing more will come.
            Err(TryRecvError::Disconnected) => {
                self.in_flight = true;
                None
            }
        }
    }

    /// Hand the reader `requests` (built only when this returns true, see
    /// [`Self::wants`]).
    pub fn send(&mut self, requests: Vec<TailRequest>, now_ms: u64) {
        if requests.is_empty() {
            return;
        }
        self.in_flight = self.requests.send(requests).is_ok();
        self.last_sent_ms = Some(now_ms);
    }

    /// Whether a frame at `now_ms` may send a batch: none is out, and the
    /// last went at least [`MIN_INTERVAL_MS`] ago.
    pub fn wants(&self, now_ms: u64) -> bool {
        !self.in_flight
            && self
                .last_sent_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= MIN_INTERVAL_MS)
    }
}
