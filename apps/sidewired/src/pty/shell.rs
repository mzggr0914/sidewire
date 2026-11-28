use super::{open_master, output::PromptEvent, write_input};
use anyhow::{Context, Result, bail};
use sidewire_protocol::ExecIdentity;
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd},
    time::Duration,
};
use tokio::io::{Interest, unix::AsyncFd};

// mksh closes extra descriptors across exec, so its ENV file is created by the
// selected identity and unlinked immediately upon sourcing. Root uses private KSU storage.
const START: &str = r#"if [ -z "$KSH_VERSION" ] || [ ! -r "$2" ] || [ ! -w "$2" ]; then exec /system/bin/sh -i; fi
case $(/system/bin/id -u) in 0) dir=/data/adb;; 2000) dir=/data/local/tmp;; *) exec /system/bin/sh -i;; esac
if [ -z "$TMPDIR" ] || [ ! -d "$TMPDIR" ] || [ ! -w "$TMPDIR" ]; then export TMPDIR="$dir"; fi
init=$(/system/bin/mktemp "$dir/.sidewire-shell.XXXXXXXX") || exec /system/bin/sh -i
if ! printf '%s\n' "$1" >"$init"; then /system/bin/rm -f "$init"; exec /system/bin/sh -i; fi
export SIDEWIRE_SAVED_ENV="$ENV" SIDEWIRE_PROMPT_TTY="$2" ENV="$init"
exec /system/bin/sh -i
"#;

pub(super) struct ShellBridge {
    control: AsyncFd<File>,
    _control_slave: File,
    main_slave: CString,
    received: u64,
    output: tokio::sync::mpsc::Sender<PromptEvent>,
    driver_at: Option<u64>,
    awaiting_line: bool,
    line_started: bool,
    partial_input: bool,
    paste_column: usize,
    shell_group: libc::pid_t,
}

fn open_slave(name: &CStr) -> io::Result<File> {
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn termios(file: &File) -> io::Result<libc::termios> {
    let mut value = std::mem::MaybeUninit::uninit();
    if unsafe { libc::tcgetattr(file.as_raw_fd(), value.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { value.assume_init() })
}

fn set_termios(file: &File, attrs: &libc::termios) -> io::Result<()> {
    if unsafe { libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, attrs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn readable(file: &File) -> io::Result<bool> {
    let mut fd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut fd, 1, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd.revents & libc::POLLIN != 0)
}

struct RestoreMode<'a>(&'a File, libc::termios);
impl Drop for RestoreMode<'_> {
    fn drop(&mut self) {
        let _ = set_termios(self.0, &self.1);
    }
}

async fn queue_token(master: &AsyncFd<File>, slave: &File, token: &[u8]) -> Result<()> {
    let original = termios(slave)?;
    let restore = RestoreMode(slave, original);
    let mut quiet = original;
    quiet.c_lflag &= !(libc::ECHO | libc::ECHONL);
    set_termios(slave, &quiet)?;
    write_input(master, token).await?;
    // The shell is blocked on fd 9. Wait for the entire canonical line to
    // reach its line discipline before restoring echo and releasing it.
    tokio::time::timeout(Duration::from_secs(2), async {
        while !readable(slave)? {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        io::Result::Ok(())
    })
    .await
    .context("queue shell parser token")??;
    drop(restore);
    Ok(())
}

impl ShellBridge {
    pub(super) fn open(
        main_slave: &CStr,
        identity: ExecIdentity,
        output: tokio::sync::mpsc::Sender<PromptEvent>,
    ) -> Result<(Self, Vec<String>)> {
        let (control, name) = open_master(80, 24)?;
        let slave = open_slave(&name)?;
        let mut attrs = termios(&slave)?;
        unsafe { libc::cfmakeraw(&mut attrs) };
        set_termios(&slave, &attrs)?;
        let uid = match identity {
            ExecIdentity::Root => 0,
            ExecIdentity::Shell => 2000,
        };
        if unsafe { libc::fchown(slave.as_raw_fd(), uid, uid) } != 0
            || unsafe { libc::fchmod(slave.as_raw_fd(), 0o600) } != 0
        {
            return Err(io::Error::last_os_error()).context("set shell control terminal owner");
        }
        let args = vec![
            "-c".into(),
            START.into(),
            "sh".into(),
            include_str!("shell.mksh").into(),
            name.to_string_lossy().into_owned(),
        ];
        Ok((
            Self {
                control,
                _control_slave: slave,
                main_slave: main_slave.to_owned(),
                received: 0,
                output,
                driver_at: None,
                awaiting_line: false,
                line_started: false,
                partial_input: false,
                paste_column: 0,
                shell_group: -1,
            },
            args,
        ))
    }

    pub(super) async fn signal(&self) -> io::Result<u8> {
        let mut byte = [0];
        let n = self
            .control
            .async_io(Interest::READABLE, |mut f| f.read(&mut byte))
            .await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(byte[0])
    }

    async fn cancel_capture(&mut self) -> Result<()> {
        if self.driver_at.take().is_some() {
            self.output
                .send(PromptEvent::End(None))
                .await
                .context("PTY output closed")?;
        }
        Ok(())
    }

    pub(super) async fn on_signal(&mut self, master: &AsyncFd<File>, signal: u8) -> Result<()> {
        match signal {
            b'P' => {
                self.shell_group = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
                let slave = open_slave(&self.main_slave)?;
                let flags = termios(&slave)?.c_lflag;
                self.cancel_capture().await?;
                let queued = readable(&slave)?;
                if flags & (libc::ICANON | libc::ECHO) == libc::ICANON | libc::ECHO
                    && !std::mem::take(&mut self.partial_input)
                    && !queued
                {
                    queue_token(master, &slave, b"__sw_read \"$?\" 9<>\"$__sw_tty\"\n").await?;
                    // mksh is still blocked in the primary prompt expansion. Do not
                    // release it until the output relay can capture the visible prompt.
                    let (ack, capturing) = tokio::sync::oneshot::channel();
                    self.output
                        .send(PromptEvent::Begin(ack))
                        .await
                        .context("PTY output closed")?;
                    capturing.await.context("PTY output closed before prompt")?;
                    self.driver_at = Some(self.received);
                }
                write_input(&self.control, if queued { b"\n" } else { b"show\n" }).await?;
            }
            b'R' if self.driver_at.take() == Some(self.received) => {
                self.awaiting_line = true;
                self.output
                    .send(PromptEvent::End(Some(self.received)))
                    .await
                    .context("PTY output closed")?;
            }
            b'R' => write_input(&self.control, b"F\n").await?,
            _ => bail!("invalid shell prompt gate"),
        }
        Ok(())
    }

    pub(super) async fn raw(
        &mut self,
        master: &AsyncFd<File>,
        data: &[u8],
        paste: bool,
    ) -> Result<()> {
        self.cancel_capture().await?;
        self.received = self.received.wrapping_add(data.len() as u64);
        if self.awaiting_line {
            self.awaiting_line = false;
            match data {
                b"\x03" => {
                    write_input(&self.control, b"C\n").await?;
                    return Ok(());
                }
                b"\x04" => {
                    write_input(&self.control, b"D\n").await?;
                    return Ok(());
                }
                _ => write_input(&self.control, b"F\n").await?,
            }
        }
        if let Some(&last) = data.last() {
            let canonical = termios(master.get_ref())?.c_lflag & libc::ICANON != 0;
            self.partial_input = canonical && !matches!(last, b'\n' | b'\r' | 3 | 4);
        }
        let attrs = termios(master.get_ref())?;
        let canonical_paste = paste
            && self.shell_group > 0
            && attrs.c_lflag & libc::ICANON != 0
            && attrs.c_cc[libc::VEOF] != 0
            && unsafe { libc::tcgetpgrp(master.as_raw_fd()) } == self.shell_group;
        let expanded;
        let mut remaining = if canonical_paste {
            expanded = canonical_paste_bytes(data, &mut self.paste_column, attrs.c_cc[libc::VEOF]);
            expanded.as_slice()
        } else {
            self.paste_column = 0;
            data
        };
        while !remaining.is_empty() {
            // A paste can fill the kernel's input buffer *while mksh waits at
            // its next prompt*. Service that gate without admitting a local
            // editor or restarting a partially completed PTY write.
            tokio::select! {
                signal = self.signal() => {
                    match signal? {
                        b'P' => {
                            let queued_line = remaining.iter().any(|b| matches!(b,b'\r'|b'\n'));
                            write_input(&self.control, if queued_line { b"\n" } else { b"show\n" }).await?;
                        }
                        b'R' => write_input(&self.control, b"F\n").await?,
                        _ => bail!("invalid shell prompt gate during input"),
                    }
                }
                result = master.async_io(Interest::WRITABLE, |mut file| std::io::Write::write(&mut file, remaining)) => {
                    match result {
                        Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                        Ok(written) => remaining = &remaining[written..],
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) async fn line(&mut self, master: &AsyncFd<File>, bytes: &[u8]) -> Result<()> {
        if !self.awaiting_line || bytes.is_empty() || bytes[..bytes.len() - 1].contains(&b'\n') {
            bail!("unexpected shell command fragment");
        }
        self.received = self.received.wrapping_add(bytes.len() as u64);
        self.paste_column = 0;
        if !self.line_started {
            let slave = open_slave(&self.main_slave)?;
            // A complete canonical line fits in 4096 bytes including LF. Send ordinary
            // commands straight to the real parser, preserving comments and shell syntax.
            if bytes.ends_with(b"\n")
                && bytes.len() <= 4096
                && bytes[..bytes.len() - 1]
                    .iter()
                    .all(|&b| b == b'\t' || (b >= 32 && b != 127))
            {
                queue_token(master, &slave, bytes).await?;
                self.awaiting_line = false;
                write_input(&self.control, b"F\n").await?;
                return Ok(());
            }
            queue_token(master, &slave, b"__sw_run\n").await?;
            write_input(&self.control, b"L").await?;
            self.line_started = true;
        }
        write_input(&self.control, bytes).await?;
        if bytes.ends_with(b"\n") {
            self.awaiting_line = false;
            self.line_started = false;
        }
        Ok(())
    }
}

// Linux N_TTY retains 4095 bytes per canonical record. VEOF after a nonempty
// prefix releases those bytes to mksh without inserting a byte or a newline.
// Only apply this to clipboard text entered at our prompt. Full-screen
// programs and other foreground process groups receive their original bytes.
fn canonical_paste_bytes(data: &[u8], column: &mut usize, eof: u8) -> Vec<u8> {
    let mut input = Vec::with_capacity(data.len() + 2);
    for &byte in data {
        if matches!(byte, b'\r' | b'\n' | 3 | 4 | 8 | 21 | 127) || byte == eof {
            *column = 0;
        } else {
            if *column >= 3072 && byte & 0xc0 != 0x80 {
                input.push(eof);
                *column = 0;
            }
            *column += 1;
        }
        input.push(byte);
    }
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_chunk_boundaries_preserve_unicode_and_newlines() {
        let original = format!("{}\r\r{}", "가🙂".repeat(2000), "x".repeat(3072));
        let mut column = 0;
        let encoded: Vec<_> = original
            .as_bytes()
            .chunks(177)
            .flat_map(|s| canonical_paste_bytes(s, &mut column, 4))
            .collect();
        assert!(encoded.contains(&4));
        assert!(
            encoded
                .split(|&b| b == 4)
                .all(|s| std::str::from_utf8(s).is_ok())
        );
        assert_eq!(
            encoded.into_iter().filter(|&b| b != 4).collect::<Vec<_>>(),
            original.as_bytes()
        );
        assert_eq!(column, 3072);
    }
}
