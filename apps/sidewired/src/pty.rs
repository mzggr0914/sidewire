mod output;
mod shell;
use crate::{command_for_identity, transport::MuxWriter, wait_for_stream_cancel};
use anyhow::{Context, Result, bail};
use shell::ShellBridge;
#[cfg(target_os = "android")]
use sidewire_protocol::PtyCompleteRequest;
use sidewire_protocol::{
    FlowKind, Frame, FrameKind, PTY_FRAME_BYTES, PTY_WINDOW_REFILL, PtyExit, PtyOpenAck,
    PtyOpenRequest, PtyResize, decode, frame,
};
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{self, Write},
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    process::Stdio,
};
use tokio::{
    io::{Interest, unix::AsyncFd},
    sync::{mpsc, watch},
};

fn set_size(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_master(cols: u16, rows: u16) -> Result<(AsyncFd<File>, CString)> {
    let fd = unsafe {
        libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK)
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("posix_openpt");
    }
    let master = unsafe { File::from_raw_fd(fd) };
    if unsafe { libc::grantpt(fd) } < 0 {
        return Err(io::Error::last_os_error()).context("grantpt");
    }
    if unsafe { libc::unlockpt(fd) } < 0 {
        return Err(io::Error::last_os_error()).context("unlockpt");
    }
    let mut name = [0 as libc::c_char; 128];
    let rc = unsafe { libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc)).context("ptsname_r");
    }
    set_size(fd, cols, rows).context("set initial PTY size")?;
    let slave_name = unsafe { CStr::from_ptr(name.as_ptr()).to_owned() };
    // The File owns the nonblocking descriptor for the lifetime of AsyncFd.
    let master = unsafe { AsyncFd::register(master) }.context("register PTY master")?;
    Ok((master, slave_name))
}

fn configure_child(slave_name: &CStr, echo: bool) -> io::Result<()> {
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { libc::open(slave_name.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut attrs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if echo {
        attrs.c_lflag |= libc::ECHO;
    } else {
        attrs.c_lflag &= !(libc::ECHO | libc::ECHONL);
    }
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &attrs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::ioctl(fd, libc::TIOCSCTTY as _, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(fd, target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if fd <= libc::STDERR_FILENO {
        // Keep the slave open if it already occupies a standard descriptor.
        let _ = slave.into_raw_fd();
        if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn terminate_group(pid: u32) {
    if pid != 0 {
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGHUP);
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

async fn write_input(master: &AsyncFd<File>, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match master
            .async_io(Interest::WRITABLE, |mut file| file.write(bytes))
            .await
        {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) async fn run(
    writer: &MuxWriter,
    stream_id: u32,
    request: PtyOpenRequest,
    inbox: &mut mpsc::Receiver<Frame>,
    mut cancel: watch::Receiver<bool>,
    hostname: &str,
    local_edit: bool,
) -> Result<()> {
    let (master, slave_name) = open_master(request.cols, request.rows)?;
    let interactive =
        request.program == "/system/bin/sh" && request.args.is_empty() && request.echo;
    let (prompt_tx, prompts) = mpsc::channel(2);
    let (mut bridge, args) = if local_edit && interactive {
        match ShellBridge::open(&slave_name, request.identity, prompt_tx) {
            Ok((bridge, args)) => (Some(bridge), args),
            Err(error) => {
                tracing::warn!(%error, "using native PTY editing");
                (None, request.args)
            }
        }
    } else {
        (None, request.args)
    };
    let mut command = command_for_identity(&request.program, &args, request.identity);
    command
        .env("TERM", &request.term)
        .env("COLORTERM", "truecolor")
        .env("HOSTNAME", hostname)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let echo = request.echo;
    unsafe { command.pre_exec(move || configure_child(&slave_name, echo)) };
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn PTY program {}", request.program))?;
    let pid = child.id().unwrap_or(0);
    #[cfg(target_os = "android")]
    let _activity = interactive.then(|| writer.shell_lease()).flatten();
    let flow_control = writer.supports_flow(FlowKind::Pty);

    let session = async {
        writer
            .send(&frame(
                FrameKind::PtyOpenAck,
                stream_id,
                &PtyOpenAck { pid },
            )?)
            .await?;
        let output_loop = output::relay(&master, writer, stream_id, prompts);
        let input_loop = async {
            let mut consumed = 0;
            loop {
                let incoming = tokio::select! {
                    signal = async {
                        match &bridge {
                            Some(shell) => shell.signal().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        let shell = bridge.as_mut().expect("active shell gate");
                        shell.on_signal(&master, signal?).await?;
                        continue;
                    }
                    incoming = inbox.recv() => incoming,
                };
                let Some(incoming) = incoming else {
                    bail!("device connection closed while PTY {stream_id} was active");
                };
                if incoming.stream_id != stream_id {
                    bail!(
                        "unexpected stream {} while PTY {stream_id} is active",
                        incoming.stream_id
                    );
                }
                let credited = flow_control
                    && FlowKind::Pty.frame_cost(incoming.kind, incoming.payload.len())? != 0;
                match incoming.kind {
                    FrameKind::PtyInput | FrameKind::ShellPaste => {
                        if let Some(shell) = &mut bridge {
                            shell
                                .raw(
                                    &master,
                                    &incoming.payload,
                                    incoming.kind == FrameKind::ShellPaste,
                                )
                                .await?;
                        } else {
                            write_input(&master, &incoming.payload)
                                .await
                                .context("write PTY master")?;
                        }
                    }
                    FrameKind::ShellLine => {
                        bridge
                            .as_mut()
                            .context("local editing is unavailable")?
                            .line(&master, &incoming.payload)
                            .await?;
                    }
                    FrameKind::PtyResize => {
                        let size: PtyResize = decode(&incoming.payload)?;
                        set_size(master.as_raw_fd(), size.cols, size.rows)?;
                    }
                    #[cfg(target_os = "android")]
                    FrameKind::PtyComplete => {
                        let request: PtyCompleteRequest = decode(&incoming.payload)?;
                        let foreground = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
                        let completion_pid = if bridge.is_some() && foreground > 0 {
                            foreground as u32
                        } else {
                            pid
                        };
                        let completion = tokio::task::spawn_blocking(move || {
                            crate::completion::complete_pty_path(completion_pid, &request)
                        })
                        .await
                        .context("join PTY completion worker")??;
                        writer
                            .send(&frame(
                                FrameKind::PtyCompleteResult,
                                stream_id,
                                &completion,
                            )?)
                            .await?;
                    }
                    FrameKind::PtyClose => return Ok::<(), anyhow::Error>(()),
                    kind => bail!("unexpected PTY frame {kind:?}"),
                }
                if credited {
                    consumed += 1;
                    if consumed == PTY_WINDOW_REFILL {
                        writer
                            .send(&FlowKind::Pty.window_update(stream_id, consumed)?)
                            .await?;
                        consumed = 0;
                    }
                }
            }
        };

        tokio::select! {
            result = output_loop => result,
            result = input_loop => {
                result?;
                terminate_group(pid);
                Ok(())
            }
        }
    };
    let result = tokio::select! {
        result = session => result,
        _ = wait_for_stream_cancel(&mut cancel) => Err(anyhow::anyhow!("PTY {stream_id} canceled")),
    };
    if result.is_err() {
        terminate_group(pid);
        let _ = child.start_kill();
    }
    let status = child.wait().await?;
    result?;
    writer
        .send(&frame(
            FrameKind::PtyExit,
            stream_id,
            &PtyExit {
                code: status.code(),
            },
        )?)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidewire_protocol::{ExecIdentity, PTY_WINDOW_FRAMES, SecureFrameReader, raw_frame};
    use tokio::{
        net::{TcpListener, TcpStream},
        time::{Duration, timeout},
    };

    #[tokio::test]
    async fn nonblocking_pty_round_trips_a_paste_larger_than_its_windows() {
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (connected, accepted) = tokio::join!(
                TcpStream::connect(listener.local_addr().unwrap()),
                listener.accept(),
            );
            let (_, writer) = connected.unwrap().into_split();
            let writer = MuxWriter::new(writer, None, true, false, true);
            let mut reader = SecureFrameReader::new(accepted.unwrap().0, None);
            let stream_id = 1;
            writer
                .register_flow_stream(stream_id, FlowKind::Pty)
                .unwrap();
            let (input, mut inbox) = mpsc::channel(16);
            let (_cancel, canceled) = watch::channel(false);
            let paste = "한🙂\r\n".repeat(14_000).into_bytes();
            let request = PtyOpenRequest {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    format!(
                        "stty raw -echo && printf ready && exec head -c {}",
                        paste.len()
                    ),
                ],
                identity: ExecIdentity::Root,
                cols: 80,
                rows: 24,
                term: "xterm".into(),
                echo: false,
            };
            let pty = run(
                &writer,
                stream_id,
                request,
                &mut inbox,
                canceled,
                "sidewire-test",
                false,
            );
            let peer = async {
                assert_eq!(
                    reader.read_frame().await.unwrap().kind,
                    FrameKind::PtyOpenAck
                );
                let ready = reader.read_frame().await.unwrap();
                assert_eq!(ready.kind, FrameKind::PtyOutput);
                assert_eq!(ready.payload, b"ready");

                let mut credit = PTY_WINDOW_FRAMES;
                let mut consumed = 1;
                let mut chunks = paste.chunks(PTY_FRAME_BYTES);
                let mut echoed = Vec::new();
                loop {
                    while credit != 0 {
                        let Some(chunk) = chunks.next() else { break };
                        input
                            .send(raw_frame(FrameKind::PtyInput, stream_id, chunk.to_vec()))
                            .await
                            .unwrap();
                        credit -= 1;
                    }
                    let received = reader.read_frame().await.unwrap();
                    match received.kind {
                        FrameKind::PtyWindowUpdate => {
                            let (kind, frames) = FlowKind::decode_window_update(&received).unwrap();
                            assert_eq!(kind, FlowKind::Pty);
                            credit += frames as usize;
                        }
                        FrameKind::PtyOutput => {
                            echoed.extend_from_slice(&received.payload);
                            consumed += 1;
                            if consumed == PTY_WINDOW_REFILL {
                                assert!(
                                    writer
                                        .add_flow_credit(stream_id, FlowKind::Pty, consumed)
                                        .unwrap()
                                );
                                consumed = 0;
                            }
                        }
                        FrameKind::PtyExit => {
                            assert_eq!(echoed, paste);
                            assert_eq!(decode::<PtyExit>(&received.payload).unwrap().code, Some(0));
                            break;
                        }
                        kind => panic!("unexpected frame {kind:?}"),
                    }
                }
            };
            let (result, ()) = tokio::join!(pty, peer);
            result.unwrap();
        })
        .await
        .unwrap();
    }
}
