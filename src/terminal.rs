use anyhow::{Context, Result, anyhow};
use std::{
    env, fs,
    io::{self, Read, Write},
    mem,
    os::fd::AsRawFd,
    sync, thread, time,
};

macro_rules! onerr {
    ($e:expr, $s:block) => {{
        match $e {
            Ok(v) => v,
            Err(_) => $s,
        }
    }};
}

#[derive(Debug)]
pub enum TerminalEscape {
    LeftArrow,
    RightArrow,
    CtrlLeftArrow,
    CtrlRightArrow,
    Timeout,
}

#[derive(Debug)]
pub enum TerminalInput {
    Printable(u8),
    Ctrl(u8),
    Escape(TerminalEscape),
    Delete,
}

#[derive(Debug)]
pub struct TerminalReader {
    tty: fs::File,
}

impl TerminalReader {
    pub fn new() -> Result<Self> {
        let tty = fs::File::open("/dev/tty")?;
        Ok(Self { tty })
    }

    fn read_u8(&mut self) -> Result<u8> {
        let mut buf = [0];
        match self.tty.read(&mut buf)? {
            0 => Err(anyhow!("unexpected eof")),
            _ => Ok(buf[0]),
        }
    }

    fn read_u8_timeout(&mut self, timeout_ms: i32) -> Result<Option<u8>> {
        let mut pollfd = libc::pollfd {
            fd: self.tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };

        let polled = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        match polled {
            0 => Ok(None),
            -1 => Err(anyhow!("error in poll")),
            _ => self.read_u8().map(|v| Some(v)),
        }
    }

    fn is_escape_end(ch: u8) -> bool {
        (0x40..=0x7e).contains(&ch)
    }

    // https://en.wikipedia.org/wiki/ANSI_escape_code#Control_Sequence_Introducer_commands
    // For Control Sequence Introducer, or CSI, commands, the ESC [ (written as \e[, \x1b[ or \033[ in several programming languages)
    // is followed by any number (including none) of "parameter bytes" in the range 0x30–0x3F (ASCII 0–9:;<=>?),
    // then by any number of "intermediate bytes" in the range 0x20–0x2F (ASCII space and !"#$%&'()*+, -./),
    // then finally by a single "final byte" in the range 0x40–0x7E (ASCII @A–Z[\]^_`a–z{|}~)
    //
    // All common sequences just use the parameters as a series of semicolon-separated numbers such as 1;2;3.
    // Missing numbers are treated as 0 (1;;3 acts like the middle number is 0, and no parameters at all in ESC[m acts like a 0 reset code).
    // Some sequences (such as CUU) treat 0 as 1 in order to make missing parameters useful.
    fn read_escape_to_end(&mut self) -> Result<String> {
        let mut string = String::new();
        loop {
            let read = self.read_u8()?;
            string.push(read as char);
            if Self::is_escape_end(read) {
                break;
            }
        }
        Ok(string)
    }

    // ^[
    fn read_escape(&mut self) -> Result<Option<TerminalEscape>> {
        let Some(next) = self.read_u8_timeout(50)? else {
            return Ok(Some(TerminalEscape::Timeout));
        };
        if next != b'[' {
            return Err(anyhow!("unexpected: {:x}", next));
        };

        let escape = self.read_escape_to_end()?;

        Ok(match escape.as_str() {
            "D" => Some(TerminalEscape::LeftArrow),
            "C" => Some(TerminalEscape::RightArrow),
            "1;5D" => Some(TerminalEscape::CtrlLeftArrow),
            "1;5C" => Some(TerminalEscape::CtrlRightArrow),
            _ => None,
        })
    }

    pub fn read_input(&mut self) -> Result<Option<TerminalInput>> {
        let mut buf = [0];
        match self.tty.read(&mut buf)? {
            0 => Ok(None),
            _ => Ok(match buf[0] {
                0x1b => self.read_escape()?.map(|v| TerminalInput::Escape(v)),
                0x9B => todo!(),
                0x90 => todo!(),
                0x9D => todo!(),
                0x7F => Some(TerminalInput::Delete),
                1..=26 => Some(TerminalInput::Ctrl(97 + buf[0] - 1)),
                x => Some(TerminalInput::Printable(x)),
            }),
        }
    }
}

pub struct TerminalWriter {
    tty: io::BufWriter<fs::File>,
    fd: i32,
    original_termios: libc::termios,
    debug: bool,
}

impl TerminalWriter {
    pub fn new() -> Result<Self> {
        let tty = fs::File::create("/dev/tty")?;
        let fd = tty.as_raw_fd();
        let mut tty = io::BufWriter::new(tty);

        let debug = env::var("TIP_DEBUG").unwrap_or("".to_string()) == "true";
        if !debug {
            switch_to_alternate_terminal(&mut tty)?
        };

        Ok(Self {
            tty,
            fd,
            original_termios: unsafe { enable_raw_mode(fd) },
            debug,
        })
    }

    fn flush(&mut self) -> Result<()> {
        self.tty.flush()?;
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<()> {
        self.write("\x1b[?25l".as_bytes())?;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        self.write("\x1b[?25h".as_bytes())?;
        Ok(())
    }

    fn newline_start(&mut self) -> Result<()> {
        self.write("\r\n".as_bytes())?;
        Ok(())
    }

    fn write(&mut self, buf: &[u8]) -> Result<()> {
        self.tty.write_all(buf)?;
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        // reset cursor, reset colors, reset styles
        self.write("\x1b[2J\x1b[H\x1b[39m\x1b[49m\x1b[0m".as_bytes())
    }

    fn move_cursor(&mut self, line: usize, column: usize) -> Result<()> {
        self.write(format!("\x1b[{};{}H", line, column).as_bytes())
    }

    fn size(&self) -> libc::winsize {
        get_terminal_size(self.fd)
    }
}

impl Drop for TerminalWriter {
    fn drop(&mut self) {
        unsafe {
            disable_raw_mode(self.fd, self.original_termios);
        }
        if !self.debug {
            let _ = switch_to_normal_terminal(&mut self.tty);
        }
    }
}

// returns the original one
unsafe fn enable_raw_mode(tty_fd: i32) -> libc::termios {
    let mut original_termios = mem::MaybeUninit::<libc::termios>::uninit();
    unsafe { libc::tcgetattr(tty_fd, original_termios.as_mut_ptr()) };
    let original_termios = unsafe { original_termios.assume_init() };

    let mut raw_termios = mem::MaybeUninit::<libc::termios>::uninit();
    unsafe { libc::cfmakeraw(raw_termios.as_mut_ptr()) };
    let raw_termios = unsafe { raw_termios.assume_init() };

    unsafe { libc::tcsetattr(tty_fd, libc::TCSAFLUSH, &raw_termios) };

    original_termios
}

fn switch_to_alternate_terminal<T: Write>(tty: &mut T) -> Result<()> {
    tty.write_all("\x1b[?1049h\x1b[2J\x1b[H".as_bytes())?;
    Ok(())
}

fn switch_to_normal_terminal<T: Write>(tty: &mut T) -> Result<()> {
    tty.write_all("\x1b[2J\x1b[H\x1b[?1049l".as_bytes())?;
    Ok(())
}

unsafe fn disable_raw_mode(tty_fd: i32, termios: libc::termios) {
    unsafe { libc::tcsetattr(tty_fd, libc::TCSAFLUSH, &termios) };
}

pub fn isatty(fd: i32) -> bool {
    let tty = unsafe { libc::isatty(fd) };
    tty == 1
}

pub fn get_terminal_size(tty_fd: i32) -> libc::winsize {
    let mut winsize = mem::MaybeUninit::<libc::winsize>::uninit();
    unsafe { libc::ioctl(tty_fd, libc::TIOCGWINSZ, winsize.as_mut_ptr()) };
    unsafe { winsize.assume_init() }
}

struct TerminalRenderState {
    left_lines: usize,
    cursor_line: usize,
    cursor_col: usize,
}

impl TerminalRenderState {
    fn new(size: &libc::winsize) -> Self {
        Self {
            left_lines: size.ws_row as usize,
            cursor_line: 1,
            cursor_col: 1,
        }
    }
}

enum ComponentRenderOut {
    Prompt(ComponentPromptOut),
    Data(ComponentDataOut),
}

pub struct ComponentDataOut(pub Vec<u8>);

pub struct ComponentPromptOut {
    pub query: Vec<char>,
    pub cursor_index: usize,
}

pub trait ComponentPrompt {
    fn input(&mut self, input: &TerminalInput) -> Result<()>;
    fn render(&self) -> ComponentPromptOut;
}

pub trait ComponentData {
    fn render(&self) -> ComponentDataOut;
}

pub enum Component<'a> {
    Prompt(&'a mut dyn ComponentPrompt),
    Data(&'a mut dyn ComponentData),
}

enum TerminalRendererEvent {
    Resize,
    Input(TerminalInput),
    Redraw,
    Quit,
}

pub struct TerminalRenderer<'a> {
    components: Vec<Component<'a>>,
    size: libc::winsize,
    terminal_writer: TerminalWriter,

    event_rx: sync::mpsc::Receiver<TerminalRendererEvent>,
}

impl<'a> TerminalRenderer<'a> {
    pub fn new(
        components: Vec<Component<'a>>,
        redraw_rx: sync::mpsc::Receiver<()>,
    ) -> Result<Self> {
        let (event_tx, event_rx) = sync::mpsc::sync_channel(0);

        // signals
        thread::spawn({
            let event_tx = event_tx.clone();
            let mut signals = signal_hook::iterator::Signals::new(&[
                signal_hook::consts::SIGWINCH,
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
            ])?;

            move || {
                for signal in &mut signals {
                    match signal {
                        libc::SIGWINCH => {
                            onerr!(event_tx.send(TerminalRendererEvent::Resize), { break })
                        }
                        libc::SIGINT | libc::SIGTERM => {
                            onerr!(event_tx.send(TerminalRendererEvent::Quit), { break })
                        }
                        _ => unreachable!(),
                    }
                }
            }
        });

        // input
        thread::spawn({
            let event_tx = event_tx.clone();
            let mut terminal_reader = TerminalReader::new()?;
            move || {
                loop {
                    let input = terminal_reader.read_input().unwrap();
                    if let Some(input) = input {
                        onerr!(event_tx.send(TerminalRendererEvent::Input(input)), {
                            break;
                        });
                    }
                }
            }
        });

        // pipe redraw
        thread::spawn({
            let event_tx = event_tx.clone();
            move || {
                let throttle = Throttle::new(move || {
                    let _ = event_tx.send(TerminalRendererEvent::Redraw);
                });
                loop {
                    onerr!(redraw_rx.recv(), { break });
                    throttle.call();
                }
            }
        });

        let terminal_writer = TerminalWriter::new()?;
        let size = terminal_writer.size();

        Ok(Self {
            size,
            terminal_writer,
            components,
            event_rx,
        })
    }

    fn handle_size(&mut self) {
        self.size = self.terminal_writer.size();
    }

    fn window_str(source: &[char], size: usize, index: usize) -> &[char] {
        if index < size {
            let end = size.min(source.len());
            return &source[..end];
        }

        &source[(index - size)..index]
    }

    fn render_component_prompt(
        &mut self,
        out: ComponentPromptOut,
        state: &mut TerminalRenderState,
    ) -> Result<()> {
        state.left_lines -= 1;

        let window = Self::window_str(&out.query, self.size.ws_col as usize, out.cursor_index);
        self.terminal_writer
            .write(window.iter().collect::<String>().as_bytes())?;

        state.cursor_line = 1;
        state.cursor_col = out.cursor_index + 1;

        Ok(())
    }

    fn render_component_data(
        &mut self,
        out: ComponentDataOut,
        state: &mut TerminalRenderState,
    ) -> Result<()> {
        self.terminal_writer.newline_start()?;
        state.left_lines -= 1;
        self.terminal_writer
            .write("─".repeat(self.size.ws_col as usize).as_bytes())?;

        let mut lines = out.0.split(|v| *v == b'\n');
        let mut left_lines = state.left_lines as isize;
        while left_lines > 0 {
            let Some(line) = lines.next() else { break };
            let escaped_vec = EscapedVec::new(
                line.iter()
                    .filter(|v| **v != b'\r')
                    .map(|v| *v)
                    .collect::<Vec<u8>>(),
            );

            let len = escaped_vec.len();
            let takes_up_lines = (len as f32 / self.size.ws_col as f32).ceil().max(1.0) as usize;

            let mut cap = len;
            if (left_lines - takes_up_lines as isize) < 0 {
                cap = self.size.ws_col as usize * left_lines as usize;
            }
            left_lines -= takes_up_lines as isize;

            self.terminal_writer.newline_start()?;
            self.terminal_writer.write(escaped_vec.cap(cap))?;
        }
        state.left_lines = left_lines.max(0) as usize;

        Ok(())
    }

    fn rerender(&mut self) -> Result<()> {
        self.terminal_writer.clear()?;
        self.terminal_writer.hide_cursor()?;

        let rendered = self
            .components
            .iter()
            .map(|v| match v {
                Component::Prompt(x) => ComponentRenderOut::Prompt(x.render()),
                Component::Data(x) => ComponentRenderOut::Data(x.render()),
            })
            .collect::<Vec<_>>();

        let mut state = TerminalRenderState::new(&self.size);
        for x in rendered {
            match x {
                ComponentRenderOut::Prompt(x) => self.render_component_prompt(x, &mut state)?,
                ComponentRenderOut::Data(x) => self.render_component_data(x, &mut state)?,
            }
        }

        self.terminal_writer
            .move_cursor(state.cursor_line, state.cursor_col)?;
        self.terminal_writer.show_cursor()?;

        self.terminal_writer.flush()?;

        Ok(())
    }

    pub fn start(mut self, mut stop: impl FnMut(&TerminalInput) -> bool) -> Result<()> {
        loop {
            self.rerender()?;
            match self
                .event_rx
                .recv()
                .with_context(|| "main listen loop receive error")?
            {
                TerminalRendererEvent::Resize => self.handle_size(),
                TerminalRendererEvent::Input(terminal_input) => {
                    if stop(&terminal_input) {
                        break;
                    }
                    for comp in &mut self.components {
                        match comp {
                            Component::Prompt(x) => x.input(&terminal_input)?,
                            _ => {}
                        }
                    }
                }
                TerminalRendererEvent::Redraw => {}
                TerminalRendererEvent::Quit => {
                    break;
                }
            }
        }

        Ok(())
    }
}

struct Throttle {
    handle: Option<thread::JoinHandle<()>>,
    should_fire: sync::Arc<sync::atomic::AtomicBool>,
    dropped: sync::Arc<sync::atomic::AtomicBool>,
}

impl Throttle {
    fn new(func: impl Fn() + Send + 'static) -> Self {
        let should_fire = sync::Arc::new(sync::atomic::AtomicBool::new(false));
        let dropped = sync::Arc::new(sync::atomic::AtomicBool::new(false));

        let handle = thread::spawn({
            let should_fire = should_fire.clone();
            let dropped = dropped.clone();
            move || {
                loop {
                    thread::sleep(time::Duration::from_millis(30));

                    if dropped.load(sync::atomic::Ordering::Relaxed) {
                        break;
                    }

                    while should_fire.load(sync::atomic::Ordering::Relaxed) {
                        if should_fire
                            .compare_exchange(
                                true,
                                false,
                                sync::atomic::Ordering::Relaxed,
                                sync::atomic::Ordering::Relaxed,
                            )
                            .is_ok()
                        {
                            func();
                            break;
                        }
                    }
                }
            }
        });

        Self {
            handle: Some(handle),
            should_fire,
            dropped,
        }
    }

    fn call(&self) {
        self.should_fire
            .store(true, sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for Throttle {
    fn drop(&mut self) {
        self.dropped.store(true, sync::atomic::Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap();
    }
}

struct EscapedIter<'a> {
    index: usize,
    chars: &'a [u8],
    in_escape: bool,
}

impl<'a> EscapedIter<'a> {
    fn new(chars: &'a [u8]) -> Self {
        Self {
            chars,
            index: 0,
            in_escape: false,
        }
    }
}

#[derive(Debug)]
struct EscapedIterItem {
    #[allow(dead_code)]
    ch: u8,
    in_escape: bool,
}

impl<'a> Iterator for EscapedIter<'a> {
    type Item = EscapedIterItem;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(ch) = self.chars.get(self.index) else {
            return None;
        };
        self.index += 1;

        let mut in_escape = self.in_escape;
        if *ch == 0x1b {
            self.in_escape = true;
            in_escape = true;
        } else if self.in_escape && *ch != b'[' {
            if TerminalReader::is_escape_end(*ch) {
                self.in_escape = false;
            }
        }

        Some(EscapedIterItem { ch: *ch, in_escape })
    }
}

struct EscapedVec {
    unescaped: Vec<u8>,
}

impl EscapedVec {
    fn new(unescaped: Vec<u8>) -> Self {
        Self { unescaped }
    }

    fn len(&self) -> usize {
        EscapedIter::new(&self.unescaped).fold(
            0,
            |acc, curr| {
                if curr.in_escape { acc } else { acc + 1 }
            },
        )
    }

    fn cap(&self, cap: usize) -> &[u8] {
        let mut len: usize = 0;
        let mut real_len: usize = 0;

        for v in EscapedIter::new(&self.unescaped) {
            real_len += 1;
            if v.in_escape {
                continue;
            }

            len += 1;
            if len >= cap {
                break;
            }
        }

        &self.unescaped[..real_len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_vec() {
        let escaped_vec = EscapedVec::new(vec![
            // 00000000: 1b5b 306d 1b5b 3335 6d52 4541 444d 452e  .[0m.[35mREADME.
            0x1b, 0x5b, 0x30, 0x6d, 0x1b, 0x5b, 0x33, 0x35, 0x6d, 0x52, 0x45, 0x41, 0x44, 0x4d,
            0x45, 0x2e,
        ]);

        assert_eq!(escaped_vec.len(), "README.".len());
        assert_eq!(
            escaped_vec.cap("READ".len()),
            [
                0x1b, b'[', b'0', b'm', 0x1b, b'[', b'3', b'5', b'm', b'R', b'E', b'A', b'D'
            ]
        );
    }
}
