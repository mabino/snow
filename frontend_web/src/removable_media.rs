//! Generic removable media insertion polling
//!
//! (Local copy of the frontend logic shared by the floppy and CD-ROM
//! managers.)

use std::collections::VecDeque;

use snow_core::emulator::comm::EmulatorStatus;
use snow_core::emulator::Emulator;

const MEDIA_POLL_INTERVAL_TICKS: u64 = 10;

pub enum MediaInsertResult {
    Done,
    /// Kept for parity with the other frontends; the web frontends' media
    /// managers never construct it (they cannot queue for later insertion
    /// through a user prompt)
    #[allow(dead_code)]
    DoneAndWaitForStatus,
    Drop,
    Deferred,
}

pub trait MediaHandler {
    type Pending;

    const MEDIA_NAME: &'static str;

    fn consume_name(&mut self) -> Option<String>;

    fn pending_from_name(&mut self, name: String) -> Self::Pending;

    fn try_insert(
        &mut self,
        emulator: &mut Emulator,
        status: &EmulatorStatus,
        pending: &mut Self::Pending,
    ) -> MediaInsertResult;
}

pub struct MediaPolling<H: MediaHandler> {
    tick_count: u64,
    pending: VecDeque<H::Pending>,
    handler: H,
}

impl<H: MediaHandler> MediaPolling<H> {
    pub fn new(mut handler: H, pending_names: impl IntoIterator<Item = String>) -> Self {
        let pending = pending_names
            .into_iter()
            .map(|name| handler.pending_from_name(name))
            .collect();
        Self {
            tick_count: 0,
            pending,
            handler,
        }
    }

    pub fn tick(&mut self, emulator: &mut Emulator, status: Option<&EmulatorStatus>) {
        self.tick_count += 1;
        if self.tick_count.is_multiple_of(MEDIA_POLL_INTERVAL_TICKS) {
            self.handle_pending_insertions(emulator, status);
        }
    }

    fn handle_pending_insertions(
        &mut self,
        emulator: &mut Emulator,
        status: Option<&EmulatorStatus>,
    ) {
        while let Some(name) = self.handler.consume_name() {
            log::info!("Queued pending {} insertion '{}'", H::MEDIA_NAME, name);
            self.pending.push_back(self.handler.pending_from_name(name));
        }
        if !self.pending.is_empty() {
            self.flush_pending(emulator, status);
        }
    }

    fn flush_pending(&mut self, emulator: &mut Emulator, status: Option<&EmulatorStatus>) {
        let Some(status) = status else {
            return;
        };
        while let Some(mut pending) = self.pending.pop_front() {
            let result = self.handler.try_insert(emulator, status, &mut pending);
            match result {
                MediaInsertResult::Done => {}
                MediaInsertResult::DoneAndWaitForStatus => return,
                MediaInsertResult::Drop => {}
                MediaInsertResult::Deferred => {
                    self.pending.push_front(pending);
                    return;
                }
            }
        }
    }
}
