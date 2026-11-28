use crate::transport::MuxWriter;
use anyhow::{Context, Result};
use sidewire_protocol::{FrameKind, PTY_FRAME_BYTES};
use std::{
    fs::File,
    io::{self, Read},
};
use tokio::{
    io::{Interest, unix::AsyncFd},
    sync::{mpsc, oneshot},
};

pub(super) enum PromptEvent {
    Begin(oneshot::Sender<()>),
    End(Option<u64>),
}

struct Output<'a> {
    writer: &'a MuxWriter,
    stream: u32,
    held: Option<Vec<u8>>,
}

impl Output<'_> {
    async fn forward(&self, bytes: &[u8]) -> Result<()> {
        for chunk in bytes.chunks(PTY_FRAME_BYTES) {
            if !chunk.is_empty() {
                self.writer
                    .send_raw(FrameKind::PtyOutput, self.stream, chunk)
                    .await?;
            }
        }
        Ok(())
    }

    async fn accept(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(held) = &mut self.held else {
            return self.forward(bytes).await;
        };
        held.extend_from_slice(bytes);
        // Reserve eight bytes for the input watermark. Unusually long prompts
        // still stream in order, with their final bytes attached to the grant.
        let limit = PTY_FRAME_BYTES - 8;
        if held.len() > limit {
            let tail = held.split_off(held.len() - limit);
            let prefix = std::mem::replace(held, tail);
            self.forward(&prefix).await?;
        }
        Ok(())
    }

    async fn drain(&mut self, master: &AsyncFd<File>, buffer: &mut [u8]) -> Result<()> {
        // A background process can write continuously. Bound one drain so that
        // it cannot starve either the shell gate or incoming keystrokes.
        for _ in 0..16 {
            match master.get_ref().read(buffer) {
                Ok(0) => break,
                Ok(n) => self.accept(&buffer[..n]).await?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.raw_os_error() == Some(libc::EIO) =>
                {
                    break;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    async fn finish_prompt(&mut self, position: Option<u64>) -> Result<()> {
        let bytes = self.held.take().unwrap_or_default();
        if let Some(position) = position {
            let mut payload = position.to_be_bytes().to_vec();
            payload.extend_from_slice(&bytes);
            self.writer
                .send_raw(FrameKind::ShellPrompt, self.stream, &payload)
                .await
        } else {
            self.forward(&bytes).await
        }
    }
}

pub(super) async fn relay(
    master: &AsyncFd<File>,
    writer: &MuxWriter,
    stream: u32,
    mut events: mpsc::Receiver<PromptEvent>,
) -> Result<()> {
    let mut output = Output {
        writer,
        stream,
        held: None,
    };
    let mut buffer = [0; PTY_FRAME_BYTES];
    loop {
        tokio::select! {
            biased;
            Some(event) = events.recv() => {
                output.drain(master, &mut buffer).await?;
                match event {
                    PromptEvent::Begin(ack) => {
                        output.held = Some(Vec::new());
                        let _ = ack.send(());
                    }
                    PromptEvent::End(position) => output.finish_prompt(position).await?,
                }
            }
            read = master.async_io(Interest::READABLE, |mut f| f.read(&mut buffer)) => {
                match read {
                    Ok(0) => break,
                    Ok(n) => output.accept(&buffer[..n]).await?,
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e).context("read PTY master"),
                }
            }
        }
    }
    output.finish_prompt(None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidewire_protocol::SecureFrameReader;
    use std::io::Write;
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    use tokio::{
        net::{TcpListener, TcpStream},
        time::{Duration, timeout},
    };

    #[tokio::test]
    async fn visible_prompt_and_edit_grant_arrive_in_one_frame() {
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (client, server) = tokio::join!(
                TcpStream::connect(listener.local_addr().unwrap()),
                listener.accept()
            );
            let writer =
                MuxWriter::new(server.unwrap().0.into_split().1, None, false, false, false);
            let mut reader = SecureFrameReader::new(client.unwrap(), None);
            let (master, name) = super::super::open_master(80, 24).unwrap();
            let mut slave = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open(std::ffi::OsStr::from_bytes(name.to_bytes()))
                .unwrap();
            let (events, rx) = mpsc::channel(2);
            let shell = async move {
                let (ack, capturing) = oneshot::channel();
                events.send(PromptEvent::Begin(ack)).await.unwrap();
                capturing.await.unwrap();
                slave.write_all(b"real prompt> ").unwrap();
                events.send(PromptEvent::End(Some(42))).await.unwrap();
                let message = reader.read_frame().await.unwrap();
                assert_eq!(message.kind, FrameKind::ShellPrompt);
                assert_eq!(&message.payload[..8], &42u64.to_be_bytes());
                assert_eq!(&message.payload[8..], b"real prompt> ");
                drop(slave);
            };
            let (result, ()) = tokio::join!(relay(&master, &writer, 7, rx), shell);
            result.unwrap();
        })
        .await
        .unwrap();
    }
}
