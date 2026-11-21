use std::char::{REPLACEMENT_CHARACTER, decode_utf16};

#[derive(Default)]
struct Utf16Input {
    lead: Option<u16>,
}

impl Utf16Input {
    fn push(&mut self, unit: u16, repeats: u16, mut emit: impl FnMut(char, u16) -> bool) -> bool {
        let repeats = repeats.max(1);
        if let Some(lead) = self.lead.take() {
            if (0xDC00..=0xDFFF).contains(&unit) {
                let character = decode_utf16([lead, unit])
                    .next()
                    .and_then(Result::ok)
                    .unwrap_or(REPLACEMENT_CHARACTER);
                return emit(character, repeats);
            }
            if !emit(REPLACEMENT_CHARACTER, 1) {
                return false;
            }
        }
        if (0xD800..=0xDBFF).contains(&unit) {
            self.lead = Some(unit);
            true
        } else {
            emit(
                char::from_u32(unit.into()).unwrap_or(REPLACEMENT_CHARACTER),
                repeats,
            )
        }
    }
}

#[cfg(windows)]
pub(super) use native::{RawModeGuard, read_input};

#[cfg(windows)]
mod native {
    use super::Utf16Input;
    use crate::pty::ConsoleEvent;
    use anyhow::{Context, Result};
    use sidewire_protocol::PTY_FRAME_BYTES;
    use std::{
        io,
        sync::atomic::{AtomicBool, Ordering},
        time::{Duration, Instant},
    };
    use tokio::sync::mpsc::Sender;
    use windows_sys::Win32::{
        Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Console::{
                ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
                ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
                ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT, GetConsoleMode,
                GetStdHandle, INPUT_RECORD, KEY_EVENT, KEY_EVENT_RECORD, ReadConsoleInputW,
                STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
            },
            Threading::WaitForSingleObject,
        },
    };

    struct ConsoleMode {
        handle: STD_HANDLE,
        original: u32,
    }

    impl ConsoleMode {
        fn capture(handle: STD_HANDLE) -> io::Result<Self> {
            let mut original = 0;
            if unsafe { GetConsoleMode(GetStdHandle(handle), &mut original) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle, original })
        }

        fn set(&self, mode: u32) -> io::Result<()> {
            if unsafe { SetConsoleMode(GetStdHandle(self.handle), mode) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for ConsoleMode {
        fn drop(&mut self) {
            let _ = self.set(self.original);
        }
    }

    pub(in crate::pty) struct RawModeGuard {
        _input: ConsoleMode,
        _output: Option<ConsoleMode>,
        use_vt: bool,
    }

    impl RawModeGuard {
        pub(in crate::pty) fn enable() -> Result<Self> {
            let input =
                ConsoleMode::capture(STD_INPUT_HANDLE).context("read console input mode")?;
            let output = ConsoleMode::capture(STD_OUTPUT_HANDLE).ok();
            let raw = (input.original
                & !(ENABLE_ECHO_INPUT
                    | ENABLE_LINE_INPUT
                    | ENABLE_PROCESSED_INPUT
                    | ENABLE_VIRTUAL_TERMINAL_INPUT))
                | ENABLE_WINDOW_INPUT;
            let use_vt = output.as_ref().is_some_and(|output| {
                output
                    .set(
                        output.original
                            | ENABLE_PROCESSED_OUTPUT
                            | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                    )
                    .is_ok()
            }) && input.set(raw | ENABLE_VIRTUAL_TERMINAL_INPUT).is_ok();
            if !use_vt {
                input.set(raw).context("enable terminal raw mode")?;
            }
            Ok(Self {
                _input: input,
                _output: output,
                use_vt,
            })
        }

        pub(in crate::pty) fn use_vt(&self) -> bool {
            self.use_vt
        }
    }

    fn text_unit(key: &KEY_EVENT_RECORD) -> Option<u16> {
        const VK_MENU: u16 = 0x12;
        let unit = unsafe { key.uChar.UnicodeChar };
        if key.bKeyDown != 0 {
            // A zero scan code distinguishes VT NUL from a physical modifier record.
            (unit != 0 || (key.wVirtualScanCode == 0 && key.wVirtualKeyCode != VK_MENU))
                .then_some(unit)
        } else {
            // Older consoles report Alt+numpad text on the Alt key release.
            (key.wVirtualKeyCode == VK_MENU && unit != 0).then_some(unit)
        }
    }

    pub(in crate::pty) fn read_input(stop: &AtomicBool, tx: &Sender<ConsoleEvent>) -> Result<()> {
        const WAIT_MS: u32 = 50;
        let mut records = [INPUT_RECORD::default(); 128];
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut decoder = Utf16Input::default();
        let mut last_size = crossterm::terminal::size().ok();
        let mut last_size_check = Instant::now();

        while !stop.load(Ordering::Relaxed) {
            if last_size_check.elapsed() >= Duration::from_millis(WAIT_MS.into()) {
                last_size_check = Instant::now();
                if let Ok(size) = crossterm::terminal::size()
                    && last_size != Some(size)
                {
                    last_size = Some(size);
                    if tx
                        .blocking_send(ConsoleEvent::Resize {
                            cols: size.0,
                            rows: size.1,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
            match unsafe { WaitForSingleObject(handle, WAIT_MS) } {
                WAIT_TIMEOUT => continue,
                WAIT_OBJECT_0 => {}
                _ => {
                    return Err(io::Error::last_os_error())
                        .context("wait for Windows console input");
                }
            }
            let mut count = 0;
            if unsafe {
                ReadConsoleInputW(
                    handle,
                    records.as_mut_ptr(),
                    records.len() as u32,
                    &mut count,
                )
            } == 0
            {
                return Err(io::Error::last_os_error()).context("read Windows console input");
            }

            let mut input = Vec::new();
            for record in &records[..count as usize] {
                if u32::from(record.EventType) != KEY_EVENT {
                    continue;
                }
                let key = unsafe { record.Event.KeyEvent };
                if let Some(unit) = text_unit(&key)
                    && !decoder.push(unit, key.wRepeatCount, |character, repeats| {
                        let mut utf8 = [0; 4];
                        let utf8 = character.encode_utf8(&mut utf8).as_bytes();
                        for _ in 0..repeats {
                            if input.len() + utf8.len() > PTY_FRAME_BYTES
                                && tx
                                    .blocking_send(ConsoleEvent::Input(std::mem::take(&mut input)))
                                    .is_err()
                            {
                                return false;
                            }
                            input.extend_from_slice(utf8);
                        }
                        true
                    })
                {
                    return Ok(());
                }
            }
            if !input.is_empty() && tx.blocking_send(ConsoleEvent::Input(input)).is_err() {
                break;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::Console::KEY_EVENT_RECORD_0;

        #[test]
        fn vt_records_keep_nul_and_alt_text_without_duplicating_key_releases() {
            let mut key = KEY_EVENT_RECORD {
                bKeyDown: 1,
                uChar: KEY_EVENT_RECORD_0 { UnicodeChar: 0 },
                ..Default::default()
            };
            assert_eq!(text_unit(&key), Some(0));
            key.wVirtualScanCode = 0x2a;
            assert_eq!(text_unit(&key), None);
            key.bKeyDown = 0;
            key.uChar = KEY_EVENT_RECORD_0 {
                UnicodeChar: 0xd83d,
            };
            assert_eq!(text_unit(&key), None);
            key.wVirtualKeyCode = 0x12;
            assert_eq!(text_unit(&key), Some(0xd83d));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_bracketed_paste_and_split_surrogates() {
        let text = "\x1b[200~한🙂\r\n다음 줄\x1b[201~";
        let mut decoder = Utf16Input::default();
        let mut output = String::new();
        for unit in text.encode_utf16() {
            decoder.push(unit, 1, |character, _| {
                output.push(character);
                true
            });
        }
        assert_eq!(output, text);
    }

    #[test]
    fn preserves_repeats_and_recovers_after_an_unpaired_surrogate() {
        let mut decoder = Utf16Input::default();
        let mut output = String::new();
        let mut emit = |character, repeats| {
            output.extend(std::iter::repeat_n(character, usize::from(repeats)));
            true
        };
        decoder.push(0xd83d, 1, &mut emit);
        decoder.push(u16::from(b'x'), 3, &mut emit);
        decoder.push(u16::from(b'\r'), 1, &mut emit);
        assert_eq!(output, "�xxx\r");
    }
}
