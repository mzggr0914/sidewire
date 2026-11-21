use anyhow::{Result, bail};
use sidewire_protocol::{
    FlowCredit, FlowKind, Frame, FrameKind, SecureFrameWriter, SharedNoise, raw_frame,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    net::tcp::OwnedWriteHalf,
    sync::{Notify, mpsc, oneshot},
};

const WRITER_QUEUE_CAPACITY: usize = 16;

struct WriterCommand {
    frame: Frame,
    done: oneshot::Sender<std::result::Result<(), String>>,
}

#[derive(Clone)]
pub(super) struct MuxWriter {
    inner: mpsc::Sender<WriterCommand>,
    canceled: Arc<StdMutex<HashSet<u32>>>,
    failed: Arc<AtomicBool>,
    failed_notify: Arc<Notify>,
    shutdown_notify: Arc<Notify>,
    flow_credits: Arc<StdMutex<HashMap<u32, Arc<FlowCredit>>>>,
    supports_stream_cancel: bool,
    supports_flow_control: bool,
    supports_pty_flow_control: bool,
}

impl MuxWriter {
    pub(super) fn new(
        writer: OwnedWriteHalf,
        noise: Option<SharedNoise>,
        supports_stream_cancel: bool,
        supports_flow_control: bool,
        supports_pty_flow_control: bool,
    ) -> Self {
        let (writer_tx, mut writer_rx) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let failed = Arc::new(AtomicBool::new(false));
        let failed_notify = Arc::new(Notify::new());
        let shutdown_notify = Arc::new(Notify::new());
        let task_failed = failed.clone();
        let task_notify = failed_notify.clone();
        let task_shutdown = shutdown_notify.clone();
        tokio::spawn(async move {
            let mut writer = SecureFrameWriter::new(writer, noise);
            loop {
                let shutdown = task_shutdown.notified();
                if task_failed.load(Ordering::Acquire) {
                    break;
                }
                let command = tokio::select! {
                    _ = shutdown => break,
                    command = writer_rx.recv() => command,
                };
                let Some(WriterCommand { frame, done }) = command else {
                    break;
                };
                let shutdown = task_shutdown.notified();
                let result = if task_failed.load(Ordering::Acquire) {
                    Err(anyhow::anyhow!("device connection writer stopped"))
                } else {
                    tokio::select! {
                        _ = shutdown => Err(anyhow::anyhow!("device connection writer stopped")),
                        result = writer.write_frame(&frame) => result,
                    }
                };
                match result {
                    Ok(()) => {
                        let _ = done.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = done.send(Err(error.to_string()));
                        if !task_failed.load(Ordering::Acquire) {
                            tracing::warn!(%error, "device mux writer ended");
                        }
                        break;
                    }
                }
            }
            task_failed.store(true, Ordering::Release);
            task_notify.notify_waiters();
        });
        Self {
            inner: writer_tx,
            canceled: Arc::new(StdMutex::new(HashSet::new())),
            failed,
            failed_notify,
            shutdown_notify,
            flow_credits: Arc::new(StdMutex::new(HashMap::new())),
            supports_stream_cancel,
            supports_flow_control,
            supports_pty_flow_control,
        }
    }

    async fn queue_frame(&self, frame: Frame) -> Result<()> {
        if self.failed.load(Ordering::Acquire) {
            bail!("device connection writer stopped");
        }
        let (done, completed) = oneshot::channel();
        self.inner
            .send(WriterCommand { frame, done })
            .await
            .map_err(|_| anyhow::anyhow!("device connection writer stopped"))?;
        match completed.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => bail!(message),
            Err(_) => bail!("device connection writer stopped"),
        }
    }

    pub(super) fn close(&self) {
        if !self.failed.swap(true, Ordering::AcqRel) {
            self.shutdown_notify.notify_waiters();
            self.failed_notify.notify_waiters();
        }
    }

    pub(super) async fn wait_failed(&self) {
        loop {
            let notified = self.failed_notify.notified();
            if self.failed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub(super) fn supports_flow(&self, kind: FlowKind) -> bool {
        match kind {
            FlowKind::File => self.supports_flow_control,
            FlowKind::Pty => self.supports_pty_flow_control,
        }
    }

    pub(super) fn register_flow_stream(&self, stream_id: u32, kind: FlowKind) -> Result<()> {
        if !self.supports_flow(kind) {
            return Ok(());
        }
        let mut credits = self.flow_credits.lock().unwrap();
        let std::collections::hash_map::Entry::Vacant(entry) = credits.entry(stream_id) else {
            bail!("stream {stream_id} already has flow control state");
        };
        entry.insert(Arc::new(FlowCredit::new(kind)));
        Ok(())
    }

    pub(super) fn add_flow_credit(
        &self,
        stream_id: u32,
        kind: FlowKind,
        credit: u32,
    ) -> Result<bool> {
        if !self.supports_flow(kind) {
            bail!("peer sent flow control update without negotiation");
        }
        let flow = self.flow_credits.lock().unwrap().get(&stream_id).cloned();
        let Some(flow) = flow else {
            return Ok(false);
        };
        if flow.kind != kind {
            bail!("window update does not match stream type");
        }
        flow.add(credit)?;
        Ok(true)
    }

    async fn acquire_flow_credit(&self, stream_id: u32, kind: FrameKind, len: usize) -> Result<()> {
        let flow_kind = match kind {
            FrameKind::FileChunk => FlowKind::File,
            FrameKind::PtyOutput | FrameKind::PtyCompleteResult => FlowKind::Pty,
            _ => return Ok(()),
        };
        if !self.supports_flow(flow_kind) {
            return Ok(());
        }
        let credit = flow_kind.frame_cost(kind, len)?;
        if credit == 0 {
            return Ok(());
        }
        let flow = self
            .flow_credits
            .lock()
            .unwrap()
            .get(&stream_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing flow control state for stream {stream_id}"))?;
        loop {
            let credit_ready = flow.notified();
            let failed = self.failed_notify.notified();
            self.ensure_active(stream_id)?;
            if self.failed.load(Ordering::Acquire) {
                bail!("device connection writer stopped");
            }
            if flow.try_take(credit)? {
                return Ok(());
            }
            tokio::select! {
                _ = credit_ready => {}
                _ = failed => {}
            }
        }
    }

    pub(super) async fn send_window_update(&self, stream_id: u32, bytes: usize) -> Result<()> {
        if !self.supports_flow_control || bytes == 0 {
            return Ok(());
        }
        let bytes = u32::try_from(bytes)
            .map_err(|_| anyhow::anyhow!("stream window update exceeds u32"))?;
        self.send(&FlowKind::File.window_update(stream_id, bytes)?)
            .await
    }

    pub(super) fn cancel_local(&self, stream_id: u32) {
        self.canceled.lock().unwrap().insert(stream_id);
        if let Some(flow) = self.flow_credits.lock().unwrap().get(&stream_id).cloned() {
            flow.notify_waiters();
        }
    }

    pub(super) fn finish_stream(&self, stream_id: u32) {
        self.canceled.lock().unwrap().remove(&stream_id);
        self.flow_credits.lock().unwrap().remove(&stream_id);
    }

    pub(super) fn is_canceled(&self, stream_id: u32) -> bool {
        self.canceled.lock().unwrap().contains(&stream_id)
    }

    fn ensure_active(&self, stream_id: u32) -> Result<()> {
        if self.is_canceled(stream_id) {
            bail!("stream {stream_id} canceled");
        }
        Ok(())
    }

    pub(super) async fn send(&self, frame: &Frame) -> Result<()> {
        self.send_raw(frame.kind, frame.stream_id, &frame.payload)
            .await
    }

    pub(super) async fn send_raw(
        &self,
        kind: FrameKind,
        stream_id: u32,
        payload: &[u8],
    ) -> Result<()> {
        self.ensure_active(stream_id)?;
        self.acquire_flow_credit(stream_id, kind, payload.len())
            .await?;
        self.ensure_active(stream_id)?;
        let result = self
            .queue_frame(raw_frame(kind, stream_id, payload.to_vec()))
            .await;
        self.ensure_active(stream_id)?;
        result
    }

    pub(super) async fn send_cancel(&self, stream_id: u32, reason: impl std::fmt::Display) {
        let message = reason.to_string();
        let kind = if self.supports_stream_cancel {
            FrameKind::StreamCancel
        } else {
            FrameKind::Error
        };
        if let Err(error) = self
            .queue_frame(raw_frame(kind, stream_id, message.into_bytes()))
            .await
        {
            tracing::debug!(stream_id, %error, "failed to send stream cancellation");
        }
    }

    pub(super) async fn send_error(&self, stream_id: u32, error: impl std::fmt::Display) {
        if self.is_canceled(stream_id) {
            return;
        }
        let message = error.to_string();
        let _ = self
            .send_raw(FrameKind::Error, stream_id, message.as_bytes())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::{FlowKind, MuxWriter};
    use sidewire_protocol::{FrameKind, PTY_WINDOW_FRAMES, SecureFrameReader};
    use tokio::{
        net::{TcpListener, TcpStream},
        time::{Duration, timeout},
    };

    async fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        (client.unwrap(), accepted.unwrap().0)
    }

    #[tokio::test]
    async fn flow_control_blocks_file_chunks_until_credit() {
        let (client, server) = tcp_pair().await;
        let (_, write_half) = client.into_split();
        let writer = MuxWriter::new(write_half, None, true, true, false);
        writer.register_flow_stream(11, FlowKind::File).unwrap();
        let sending = {
            let writer = writer.clone();
            tokio::spawn(async move {
                writer
                    .send_raw(FrameKind::FileChunk, 11, b"data")
                    .await
                    .unwrap();
            })
        };
        let mut reader = SecureFrameReader::new(server, None);
        assert!(
            timeout(Duration::from_millis(50), reader.read_frame())
                .await
                .is_err()
        );
        assert!(writer.add_flow_credit(11, FlowKind::File, 3).unwrap());
        assert!(
            timeout(Duration::from_millis(50), reader.read_frame())
                .await
                .is_err()
        );
        assert!(writer.add_flow_credit(11, FlowKind::File, 1).unwrap());
        let chunk = timeout(Duration::from_secs(1), reader.read_frame())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chunk.kind, FrameKind::FileChunk);
        assert_eq!(chunk.payload, b"data");
        sending.await.unwrap();
        writer.finish_stream(11);
    }

    #[tokio::test]
    async fn flow_control_wait_is_canceled_locally() {
        let (client, _server) = tcp_pair().await;
        let (_, write_half) = client.into_split();
        let writer = MuxWriter::new(write_half, None, true, true, false);
        writer.register_flow_stream(12, FlowKind::File).unwrap();
        let sending = {
            let writer = writer.clone();
            tokio::spawn(async move { writer.send_raw(FrameKind::FileChunk, 12, b"blocked").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        writer.cancel_local(12);
        let error = timeout(Duration::from_secs(1), sending)
            .await
            .expect("flow-control waiter did not wake after cancellation")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("canceled"));
        writer.finish_stream(12);
    }

    #[tokio::test]
    async fn pty_output_uses_frame_credit_and_cancel_wakes_the_sender() {
        let (client, server) = tcp_pair().await;
        let (_, write_half) = client.into_split();
        let writer = MuxWriter::new(write_half, None, true, false, true);
        writer.register_flow_stream(12, FlowKind::Pty).unwrap();
        let sending = {
            let writer = writer.clone();
            tokio::spawn(async move {
                for _ in 0..PTY_WINDOW_FRAMES {
                    writer
                        .send_raw(FrameKind::PtyOutput, 12, b"x")
                        .await
                        .unwrap();
                }
                writer.send_raw(FrameKind::PtyOutput, 12, b"blocked").await
            })
        };
        let mut reader = SecureFrameReader::new(server, None);
        for _ in 0..PTY_WINDOW_FRAMES {
            let received = timeout(Duration::from_secs(1), reader.read_frame())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(received.kind, FrameKind::PtyOutput);
            assert_eq!(received.payload, b"x");
        }
        assert!(
            timeout(Duration::from_millis(30), reader.read_frame())
                .await
                .is_err()
        );

        writer.send_raw(FrameKind::Pong, 13, b"ok").await.unwrap();
        let pong = timeout(Duration::from_secs(1), reader.read_frame())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pong.kind, FrameKind::Pong);
        writer.cancel_local(12);
        let error = timeout(Duration::from_secs(1), sending)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("canceled"));
    }

    #[tokio::test]
    async fn cancel_is_stream_scoped_and_signaled() {
        let (client, server) = tcp_pair().await;
        let (_, write_half) = client.into_split();
        let writer = MuxWriter::new(write_half, None, true, false, false);
        let mut reader = SecureFrameReader::new(server, None);

        writer.cancel_local(7);
        writer.send_cancel(7, "overflow").await;
        let frame = reader.read_frame().await.unwrap();
        assert_eq!(frame.kind, FrameKind::StreamCancel);
        assert_eq!(frame.stream_id, 7);
        assert_eq!(frame.payload, b"overflow");
        assert!(
            writer
                .send_raw(FrameKind::FileChunk, 7, b"late")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn legacy_peer_gets_error_instead_of_unknown_cancel_kind() {
        let (client, server) = tcp_pair().await;
        let (_, write_half) = client.into_split();
        let writer = MuxWriter::new(write_half, None, false, false, false);
        let mut reader = SecureFrameReader::new(server, None);

        writer.send_cancel(9, "overflow").await;
        let frame = reader.read_frame().await.unwrap();
        assert_eq!(frame.kind, FrameKind::Error);
        assert_eq!(frame.stream_id, 9);
        assert_eq!(frame.payload, b"overflow");
    }
}
