use crate::{Frame, FrameKind, PtyWindowUpdate, StreamWindowUpdate, decode, frame};
use anyhow::{Context, Result, bail};
use std::sync::Mutex;
use tokio::sync::{Notify, futures::Notified};

pub const PTY_FRAME_BYTES: usize = 4 * 1024;
// Each direction starts with this many frames. Both receive queues have
// 16 slots, leaving room for open, close, and final status frames.
pub const PTY_WINDOW_FRAMES: usize = 12;
pub const PTY_WINDOW_REFILL: u32 = PTY_WINDOW_FRAMES as u32 / 2;
const MAX_FILE_CREDIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowKind {
    File,
    Pty,
}

impl FlowKind {
    fn limit(self) -> usize {
        match self {
            Self::File => MAX_FILE_CREDIT,
            Self::Pty => PTY_WINDOW_FRAMES,
        }
    }

    pub fn frame_cost(self, kind: FrameKind, len: usize) -> Result<usize> {
        match (self, kind) {
            (Self::File, FrameKind::FileChunk) => Ok(len),
            (
                Self::Pty,
                FrameKind::PtyInput
                | FrameKind::PtyOutput
                | FrameKind::ShellLine
                | FrameKind::ShellPaste,
            ) => {
                if len > PTY_FRAME_BYTES {
                    bail!("PTY data frame exceeds {PTY_FRAME_BYTES} bytes");
                }
                Ok(1)
            }
            (
                Self::Pty,
                FrameKind::PtyResize
                | FrameKind::PtyComplete
                | FrameKind::PtyCompleteResult
                | FrameKind::ShellPrompt,
            ) => Ok(1),
            _ => Ok(0),
        }
    }

    pub fn window_update(self, stream_id: u32, credit: u32) -> Result<Frame> {
        match self {
            Self::File => frame(
                FrameKind::StreamWindowUpdate,
                stream_id,
                &StreamWindowUpdate { bytes: credit },
            ),
            Self::Pty => frame(
                FrameKind::PtyWindowUpdate,
                stream_id,
                &PtyWindowUpdate { frames: credit },
            ),
        }
    }

    pub fn decode_window_update(frame: &Frame) -> Result<(Self, u32)> {
        Ok(match frame.kind {
            FrameKind::StreamWindowUpdate => (
                Self::File,
                decode::<StreamWindowUpdate>(&frame.payload)?.bytes,
            ),
            FrameKind::PtyWindowUpdate => {
                (Self::Pty, decode::<PtyWindowUpdate>(&frame.payload)?.frames)
            }
            _ => bail!("expected a stream window update"),
        })
    }
}

pub struct FlowCredit {
    pub kind: FlowKind,
    available: Mutex<usize>,
    notify: Notify,
}

impl FlowCredit {
    pub fn new(kind: FlowKind) -> Self {
        Self {
            kind,
            available: Mutex::new(match kind {
                FlowKind::File => 0,
                FlowKind::Pty => PTY_WINDOW_FRAMES,
            }),
            notify: Notify::new(),
        }
    }

    pub fn add(&self, credit: u32) -> Result<()> {
        if credit == 0 {
            bail!("stream window update must grant positive credit");
        }
        let mut available = self.available.lock().unwrap();
        let updated = available
            .checked_add(credit as usize)
            .context("stream flow credit overflow")?;
        let limit = self.kind.limit();
        if updated > limit {
            bail!("stream flow credit exceeds {limit}");
        }
        *available = updated;
        drop(available);
        self.notify_waiters();
        Ok(())
    }

    pub fn try_take(&self, credit: usize) -> Result<bool> {
        let limit = self.kind.limit();
        if credit > limit {
            bail!("frame requires more than {limit} units of flow credit");
        }
        let mut available = self.available.lock().unwrap();
        if *available < credit {
            return Ok(false);
        }
        *available -= credit;
        Ok(true)
    }

    pub fn notified(&self) -> Notified<'_> {
        self.notify.notified()
    }

    pub fn notify_waiters(&self) {
        self.notify.notify_waiters();
    }
}
