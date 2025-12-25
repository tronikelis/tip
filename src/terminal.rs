use anyhow::{Context, Result, anyhow};
use std::{
    env, fs,
    io::{self, Read, Write},
    mem,
    os::fd::AsRawFd,
    sync, thread,
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

    // ^[
    fn read_escape(&mut self) -> Result<TerminalEscape> {
        let Some(next) = self.read_u8_timeout(50)? else {
            return Ok(TerminalEscape::Timeout);
        };
        if next != b'[' {
            return Err(anyhow!("unexpected: {:x}", next));
        };

        match self.read_u8()? {
            b'D' => return Ok(TerminalEscape::LeftArrow),
            b'C' => return Ok(TerminalEscape::RightArrow),
            _ => todo!(),
        }
    }

    pub fn read_input(&mut self) -> Result<Option<TerminalInput>> {
        let mut buf = [0];
        match self.tty.read(&mut buf)? {
            0 => Ok(None),
            _ => Ok(Some(match buf[0] {
                0x1b => TerminalInput::Escape(self.read_escape()?),
                0x9B => todo!(),
                0x90 => todo!(),
                0x9D => todo!(),
                0x7F => TerminalInput::Delete,
                1..=26 => TerminalInput::Ctrl(97 + buf[0] - 1),
                x => TerminalInput::Printable(x),
            })),
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

    fn newline_start(&mut self) -> Result<()> {
        self.write("\r\n".as_bytes())?;
        Ok(())
    }

    fn write(&mut self, buf: &[u8]) -> Result<()> {
        self.tty.write_all(buf)?;
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        self.write("\x1b[2J\x1b[H".as_bytes())
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
    pub query: String,
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
                    match input {
                        Some(input) => {
                            onerr!(event_tx.send(TerminalRendererEvent::Input(input)), {
                                break;
                            });
                        }
                        None => break,
                    }
                }
            }
        });

        // pipe redraw
        thread::spawn({
            let event_tx = event_tx.clone();
            move || {
                loop {
                    onerr!(redraw_rx.recv(), { break });
                    onerr!(event_tx.send(TerminalRendererEvent::Redraw), { break });
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

    fn render_component_prompt(
        &mut self,
        out: ComponentPromptOut,
        state: &mut TerminalRenderState,
    ) -> Result<()> {
        state.left_lines -= 1;

        let mut cols = self.size.ws_col as usize;
        let chevron = "> ".as_bytes();
        cols -= chevron.len();
        self.terminal_writer.write(chevron)?;

        if out.query.len() <= cols {
            self.terminal_writer.write(out.query.as_bytes())?;
        } else {
            let offset = out.query.len() - cols;
            self.terminal_writer
                .write(out.query.as_str()[offset..].as_bytes())?;
        }

        state.cursor_line = 1;
        state.cursor_col = out.cursor_index + chevron.len() + 1;

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
            .write("-".repeat(self.size.ws_col as usize).as_bytes())?;

        let as_string = unsafe { String::from_utf8_unchecked(out.0) };

        for line in as_string.split("\n").take(state.left_lines as usize) {
            self.terminal_writer.newline_start()?;
            let max_len = line.len().min(self.size.ws_col as usize);
            self.terminal_writer.write(line[0..max_len].as_bytes())?;
        }
        state.left_lines = 0;

        Ok(())
    }

    fn rerender(&mut self) -> Result<()> {
        self.terminal_writer.clear()?;

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
