use super::{open_master, write_input};
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
case $(/system/bin/id -u) in 0) dir=/data/adb;; *) dir=/data/local/tmp;; esac
init=$(/system/bin/mktemp "$dir/.sidewire-shell.XXXXXXXX") || exec /system/bin/sh -i
if ! printf '%s\n' "$1" >"$init"; then /system/bin/rm -f "$init"; exec /system/bin/sh -i; fi
export SIDEWIRE_SAVED_ENV="$ENV" SIDEWIRE_PROMPT_TTY="$2" ENV="$init"
exec /system/bin/sh -i
"#;

pub(super) struct ShellBridge {
    control: AsyncFd<File>,
    _control_slave: File,
    main_slave: CString,
    pub(super) received: u64,
    driver_at: Option<u64>,
    awaiting_line: bool,
    line_started: bool,
    partial_input: bool,
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
    pub(super) fn open(main_slave: &CStr, identity: ExecIdentity) -> Result<(Self, Vec<String>)> {
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
                driver_at: None,
                awaiting_line: false,
                line_started: false,
                partial_input: false,
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

    pub(super) async fn on_signal(&mut self, master: &AsyncFd<File>, signal: u8) -> Result<bool> {
        match signal {
            b'P' => {
                let slave = open_slave(&self.main_slave)?;
                let flags = termios(&slave)?.c_lflag;
                self.driver_at = None;
                if flags & (libc::ICANON | libc::ECHO) == libc::ICANON | libc::ECHO
                    && !std::mem::take(&mut self.partial_input)
                    && !readable(&slave)?
                {
                    queue_token(master, &slave, b"__sw_read \"$?\" 9<>\"$__sw_tty\"\n").await?;
                    self.driver_at = Some(self.received);
                }
                write_input(&self.control, b"\n").await?;
            }
            b'R' if self.driver_at.take() == Some(self.received) => {
                self.awaiting_line = true;
                return Ok(true);
            }
            b'R' => write_input(&self.control, b"F\n").await?,
            _ => bail!("invalid shell prompt gate"),
        }
        Ok(false)
    }

    pub(super) async fn raw(&mut self, master: &AsyncFd<File>, data: &[u8]) -> Result<()> {
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
        write_input(master, data).await?;
        Ok(())
    }

    pub(super) async fn line(&mut self, master: &AsyncFd<File>, bytes: &[u8]) -> Result<()> {
        if !self.awaiting_line || bytes.is_empty() || bytes[..bytes.len() - 1].contains(&b'\n') {
            bail!("unexpected shell command fragment");
        }
        self.received = self.received.wrapping_add(bytes.len() as u64);
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
