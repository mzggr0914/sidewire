use super::*;
use crate::console_output::StreamOutput;
use sidewire_protocol::{Frame, PTY_FRAME_BYTES, PTY_WINDOW_FRAMES};
use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const ID: u32 = 0x5054_5901;
const PASTE_ON: &[u8] = b"\x1b[?2004h";
const PASTE_OFF: &[u8] = b"\x1b[?2004l";
const PASTE_END: &str = "\x1b[201~";

#[derive(Default)]
struct Line {
    text: String,
    cursor: usize,
}

impl Line {
    fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }
    fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        // Inserting just before a combining mark can join two clusters.
        if self.cursor < self.text.len()
            && self.text[self.cursor..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_ascii())
        {
            self.cursor = self
                .text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .find(|&i| i >= self.cursor)
                .unwrap_or(self.text.len());
        }
    }
    fn left(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }
    fn right(&self) -> usize {
        self.cursor
            + self.text[self.cursor..]
                .graphemes(true)
                .next()
                .map_or(0, str::len)
    }
    fn backspace(&mut self) {
        let prev = self.left();
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }
}

#[derive(Default)]
struct Terminal {
    editing: bool,
    line: Line,
    history: VecDeque<String>,
    history_pos: usize,
    draft: String,
    prompt_tail: Vec<u8>,
    prompt: Vec<u8>,
    start_col: u16,
    columns: u16,
    sent: u64,
    escape: String,
    paste: bool,
    paste_tail: String,
    paste_suffix: String,
    skip_lf: bool,
    deferred_prompt: Option<u64>,
    completion: Option<(String, usize)>,
    listed: Option<(String, usize)>,
}

impl Terminal {
    fn data(&mut self, queue: &mut VecDeque<Frame>, kind: FrameKind, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.sent = self.sent.wrapping_add(bytes.len() as u64);
            queue.push_back(raw_frame(kind, ID, bytes));
        }
    }

    fn output(&mut self, out: &mut StreamOutput, bytes: &[u8]) -> Result<()> {
        if self.editing {
            out.write_chunk(b"\r\x1b[K")?;
            out.write_chunk(bytes)?;
            if !bytes.ends_with(b"\n") {
                out.write_chunk(b"\r\n")?;
            }
            out.write_chunk(&self.prompt)?;
            self.redraw(out)?;
        } else {
            out.write_chunk(bytes)?;
            if let Some(pos) = bytes.iter().rposition(|b| matches!(b, b'\n' | b'\r')) {
                self.prompt_tail.clear();
                self.prompt_tail.extend_from_slice(&bytes[pos + 1..]);
            } else {
                self.prompt_tail.extend_from_slice(bytes);
            }
            if self.prompt_tail.len() > 4096 {
                self.prompt_tail.drain(..self.prompt_tail.len() - 4096);
            }
        }
        Ok(())
    }

    fn ready(&mut self, out: &mut StreamOutput, pos: u64) -> Result<()> {
        if self.sent != pos {
            return Ok(());
        }
        if self.paste {
            self.deferred_prompt = Some(pos);
            return Ok(());
        }
        self.editing = true;
        self.line = Line::default();
        self.history_pos = self.history.len();
        self.draft.clear();
        self.prompt.clone_from(&self.prompt_tail);
        self.columns = crossterm::terminal::size().map_or(80, |v| v.0).max(2);
        self.start_col = crossterm::cursor::position()
            .map_or(0, |v| v.0)
            .min(self.columns - 1);
        if self.start_col + 1 >= self.columns {
            out.write_chunk(b"\r\n")?;
            self.start_col = 0;
        }
        out.write_chunk(PASTE_ON)?;
        Ok(())
    }

    fn redraw(&self, out: &mut StreamOutput) -> Result<()> {
        let start = self.start_col.min(self.columns.saturating_sub(2));
        let available = usize::from(self.columns.saturating_sub(start + 1)).max(1);
        let prefix = &self.line.text[..self.line.cursor];
        let mut left = self.line.cursor;
        let mut cursor_cells = 0;
        for g in prefix.graphemes(true).rev() {
            let width = g.width();
            if cursor_cells + width > available {
                break;
            }
            cursor_cells += width;
            left -= g.len();
        }
        let mut right = self.line.cursor;
        let mut used = cursor_cells;
        for g in self.line.text[right..].graphemes(true) {
            used += g.width();
            if used > available {
                break;
            }
            right += g.len();
        }
        // A one-row viewport keeps cursor restoration valid even after scrolling.
        let mut rendered = format!("\x1b[{}G\x1b[K", start + 1);
        for ch in self.line.text[left..right].chars() {
            rendered.push(if ch.is_control() { ' ' } else { ch });
        }
        use std::fmt::Write as _;
        write!(rendered, "\x1b[{}G", usize::from(start) + cursor_cells + 1)?;
        out.write_chunk(rendered.as_bytes())?;
        Ok(())
    }

    fn leave(&mut self, out: &mut StreamOutput) -> Result<()> {
        self.editing = false;
        self.completion = None;
        self.listed = None;
        self.escape.clear();
        self.prompt_tail.clear();
        out.write_chunk(PASTE_OFF)?;
        Ok(())
    }

    fn submit(&mut self, out: &mut StreamOutput, queue: &mut VecDeque<Frame>) -> Result<()> {
        self.line.cursor = self.line.text.len();
        self.redraw(out)?;
        self.leave(out)?;
        out.write_chunk(b"\r\n")?;
        let text = std::mem::take(&mut self.line.text);
        if !text.is_empty() && !text.starts_with(' ') && self.history.back() != Some(&text) {
            if self.history.len() == 128 {
                self.history.pop_front();
            }
            self.history.push_back(text.clone());
        }
        self.line.cursor = 0;
        let mut bytes = text.into_bytes();
        bytes.push(b'\n');
        self.data(queue, FrameKind::ShellLine, bytes);
        Ok(())
    }

    fn history(&mut self, up: bool) {
        if self.history_pos == self.history.len() {
            self.draft.clone_from(&self.line.text);
        }
        self.history_pos = if up {
            self.history_pos.saturating_sub(1)
        } else {
            (self.history_pos + 1).min(self.history.len())
        };
        self.line.set(
            self.history
                .get(self.history_pos)
                .unwrap_or(&self.draft)
                .clone(),
        );
    }

    fn escape_key(&mut self) {
        match self.escape.as_str() {
            "\x1b[A" | "\x1bOA" => self.history(true),
            "\x1b[B" | "\x1bOB" => self.history(false),
            "\x1b[D" | "\x1bOD" => self.line.cursor = self.line.left(),
            "\x1b[C" | "\x1bOC" => self.line.cursor = self.line.right(),
            "\x1b[H" | "\x1bOH" | "\x1b[1~" | "\x1b[7~" => self.line.cursor = 0,
            "\x1b[F" | "\x1bOF" | "\x1b[4~" | "\x1b[8~" => self.line.cursor = self.line.text.len(),
            "\x1b[3~" => {
                let end = self.line.right();
                self.line.text.drain(self.line.cursor..end);
            }
            "\x1b[200~" => self.paste = true,
            _ => {}
        }
        self.escape.clear();
    }

    fn paste_text(
        &mut self,
        text: &str,
        out: &mut StreamOutput,
        queue: &mut VecDeque<Frame>,
    ) -> Result<()> {
        let mut raw = Vec::new();
        let mut offset = 0;
        for (idx, ch) in text
            .char_indices()
            .filter(|(_, c)| matches!(c, '\r' | '\n'))
        {
            let part = &text[offset..idx];
            if !part.is_empty() {
                self.skip_lf = false;
                if self.editing {
                    self.line.insert(part);
                } else {
                    raw.extend_from_slice(part.as_bytes());
                }
            }
            offset = idx + 1;
            if ch == '\n' && self.skip_lf {
                self.skip_lf = false;
                continue;
            }
            self.skip_lf = ch == '\r';
            if self.editing {
                self.paste_suffix = self.line.text.split_off(self.line.cursor);
                self.submit(out, queue)?;
            } else {
                raw.push(b'\r');
            }
        }
        if offset < text.len() {
            self.skip_lf = false;
            if self.editing {
                self.line.insert(&text[offset..]);
            } else {
                raw.extend_from_slice(&text.as_bytes()[offset..]);
            }
        }
        self.data(queue, FrameKind::PtyInput, raw);
        Ok(())
    }

    fn input(
        &mut self,
        mut bytes: &[u8],
        out: &mut StreamOutput,
        queue: &mut VecDeque<Frame>,
    ) -> Result<()> {
        while !bytes.is_empty() {
            if self.paste {
                let mut stream = std::mem::take(&mut self.paste_tail);
                stream.push_str(std::str::from_utf8(bytes)?);
                if let Some(end) = stream.find(PASTE_END) {
                    self.paste_text(&stream[..end], out, queue)?;
                    self.paste = false;
                    let suffix = std::mem::take(&mut self.paste_suffix);
                    self.data(queue, FrameKind::PtyInput, suffix.into_bytes());
                    if let Some(pos) = self.deferred_prompt.take() {
                        self.ready(out, pos)?;
                    }
                    if self.editing {
                        self.redraw(out)?;
                    }
                    return self.input(&stream.as_bytes()[end + PASTE_END.len()..], out, queue);
                }
                let hold = (1..PASTE_END.len())
                    .rev()
                    .find(|&len| stream.ends_with(&PASTE_END[..len]))
                    .unwrap_or(0);
                self.paste_tail = stream.split_off(stream.len() - hold);
                self.paste_text(&stream, out, queue)?;
                if self.editing {
                    self.redraw(out)?;
                }
                return Ok(());
            }
            if self.skip_lf {
                self.skip_lf = false;
                if bytes[0] == b'\n' {
                    bytes = &bytes[1..];
                    continue;
                }
            }
            if !self.editing {
                self.data(queue, FrameKind::PtyInput, bytes.to_vec());
                return Ok(());
            }
            let ch = bytes[0];
            if !self.escape.is_empty() {
                if !ch.is_ascii() {
                    self.escape.clear();
                    continue;
                }
                self.escape.push(char::from(ch));
                if self.escape.len() == 2 && !matches!(ch, b'[' | b'O')
                    || self.escape.len() > 2 && (0x40..=0x7e).contains(&ch)
                    || self.escape.len() > 32
                {
                    self.escape_key();
                }
                bytes = &bytes[1..];
                continue;
            }
            if ch >= 32 && ch != 127 {
                let len = bytes
                    .iter()
                    .position(|b| *b < 32 || *b == 127)
                    .unwrap_or(bytes.len());
                self.line.insert(std::str::from_utf8(&bytes[..len])?);
                self.listed = None;
                bytes = &bytes[len..];
                continue;
            }
            bytes = &bytes[1..];
            match ch {
                27 => self.escape.push('\x1b'),
                13 | 10 => {
                    self.submit(out, queue)?;
                    self.skip_lf = ch == 13;
                }
                3 => {
                    self.leave(out)?;
                    out.write_chunk(b"^C\r\n")?;
                    self.data(queue, FrameKind::PtyInput, vec![3]);
                }
                4 if self.line.text.is_empty() => {
                    self.leave(out)?;
                    self.data(queue, FrameKind::PtyInput, vec![4]);
                }
                4 => {
                    let end = self.line.right();
                    self.line.text.drain(self.line.cursor..end);
                }
                8 | 127 => self.line.backspace(),
                1 => self.line.cursor = 0,
                5 => self.line.cursor = self.line.text.len(),
                2 => self.line.cursor = self.line.left(),
                6 => self.line.cursor = self.line.right(),
                16 => self.history(true),
                14 => self.history(false),
                21 => {
                    self.line.text.drain(..self.line.cursor);
                    self.line.cursor = 0;
                }
                11 => self.line.text.truncate(self.line.cursor),
                23 => {
                    while self.line.text[..self.line.cursor].ends_with(char::is_whitespace) {
                        self.line.backspace();
                    }
                    while self.line.cursor > 0
                        && !self.line.text[..self.line.cursor].ends_with(char::is_whitespace)
                    {
                        self.line.backspace();
                    }
                }
                9 if self.completion.is_none() => {
                    let req = PtyCompleteRequest {
                        line: self.line.text.clone(),
                        cursor: self.line.cursor as u32,
                    };
                    queue.push_back(frame(FrameKind::PtyComplete, ID, &req)?);
                    self.completion = Some((req.line, self.line.cursor));
                }
                _ => {}
            }
        }
        if self.editing {
            self.redraw(out)?;
        }
        Ok(())
    }

    fn completed(&mut self, out: &mut StreamOutput, reply: PtyCompleteResult) -> Result<()> {
        if !self.editing
            || self.completion.take().as_ref() != Some(&(self.line.text.clone(), self.line.cursor))
        {
            return Ok(());
        }
        let repeated = self
            .listed
            .as_ref()
            .is_some_and(|(s, c)| s == &self.line.text && *c == self.line.cursor);
        if repeated && reply.candidate_count > 1 {
            let list = format_completion_candidates(
                &reply.candidates,
                reply.candidate_count,
                self.columns,
            );
            out.write_chunk(format!("\r\n{list}\r\n").as_bytes())?;
            out.write_chunk(&self.prompt)?;
        }
        let cursor = reply.cursor as usize;
        if reply.line.is_char_boundary(cursor) {
            self.line.text = reply.line;
            self.line.cursor = cursor;
            self.listed = Some((self.line.text.clone(), cursor));
            self.redraw(out)?;
        }
        Ok(())
    }
}

pub(super) async fn run(control: &str, device: Option<String>, run_as: RunAs) -> Result<()> {
    let guard = RawModeGuard::enable()?;
    if !guard.use_vt() {
        drop(guard);
        return run_pty_client(control, device, run_as, "/system/bin/sh".into(), Vec::new()).await;
    }
    let stream = open_pty_control(
        control,
        device,
        run_as,
        "/system/bin/sh".into(),
        Vec::new(),
        true,
        true,
    )
    .await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (remote_tx, mut remote_rx) = tokio::sync::mpsc::channel(PTY_WINDOW_FRAMES);
    let remote_task = tokio::spawn(async move {
        loop {
            let next = read_frame(&mut reader).await;
            let failed = next.is_err();
            if remote_tx.send(next).await.is_err() || failed {
                break;
            }
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let (event_tx, mut events) = tokio::sync::mpsc::channel(PTY_WINDOW_FRAMES);
    let input_task = spawn_console_reader(stop.clone(), event_tx, true);
    let (tx, mut writes) = tokio::sync::mpsc::channel::<Frame>(PTY_WINDOW_FRAMES);
    let writing = async {
        while let Some(f) = writes.recv().await {
            if matches!(f.kind, FrameKind::ShellLine | FrameKind::PtyInput) {
                for chunk in f.payload.chunks(PTY_FRAME_BYTES) {
                    write_raw_frame(&mut writer, f.kind, ID, chunk).await?;
                }
            } else {
                write_frame(&mut writer, &f).await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let mut out = StreamOutput::stdout();
    let mut terminal = Terminal::default();
    let ui = async {
        let mut queue = VecDeque::new();
        loop {
            tokio::select! {
                slot=tx.reserve(), if !queue.is_empty()=>slot?.send(queue.pop_front().unwrap()),
                event=events.recv(), if queue.len()<PTY_WINDOW_FRAMES=>match event.context("console reader stopped")? {
                    ConsoleEvent::Input(data)=>terminal.input(&data,&mut out,&mut queue)?,
                    ConsoleEvent::Resize{cols,rows}=>{terminal.columns=cols.max(2);if terminal.editing {terminal.redraw(&mut out)?;} queue.push_back(frame(FrameKind::PtyResize,ID,&PtyResize{cols,rows})?);}
                    ConsoleEvent::Error(e)=>bail!(e),
                    ConsoleEvent::Eof=>return Ok(()),
                    ConsoleEvent::Complete{reply,..}=>{let _=reply.send(None);}
                },
                remote=remote_rx.recv()=>{let f=remote.context("PTY network reader stopped")??;
                    match f.kind {
                        FrameKind::PtyOutput=>terminal.output(&mut out,&f.payload)?,
                        FrameKind::ShellPrompt=>terminal.ready(&mut out,u64::from_be_bytes(f.payload.as_slice().try_into().context("invalid shell input position")?))?,
                        FrameKind::PtyCompleteResult=>terminal.completed(&mut out,decode(&f.payload)?)?,
                        FrameKind::PtyExit=>{let exit:PtyExit=decode(&f.payload)?; if exit.code.unwrap_or(0)!=0 {bail!("remote PTY exited (code {:?})",exit.code)} return Ok(());}
                        FrameKind::Error=>bail!("remote PTY error: {}",String::from_utf8_lossy(&f.payload)),
                        kind=>bail!("unexpected PTY frame {kind:?}"),
                    }
                }
            }
        }
    };
    let result = tokio::select! { result=ui=>result, result=writing=>result };
    stop.store(true, Ordering::Relaxed);
    drop(events);
    remote_task.abort();
    let _ = input_task.await;
    if terminal.editing || terminal.paste {
        let _ = out.write_chunk(PASTE_OFF);
    }
    out.finish()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_complete_graphemes() {
        let mut line = Line::default();
        line.set("A한👩‍💻".into());
        line.backspace();
        assert_eq!(line.text, "A한");
        line.backspace();
        assert_eq!(line.text, "A");
    }
}
