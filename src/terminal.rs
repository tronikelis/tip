use std::{
    fs,
    io::{self, Read, Write},
    mem,
    os::fd::AsRawFd,
    sync, thread,
};

#[derive(Debug)]
pub enum TerminalEscape {
    LeftArrow,
    RightArrow,
}

#[derive(Debug)]
pub enum TerminalInput {
    Printable(u8),
    Ctrl(u8),
    Escape(TerminalEscape),
    Esc,
    Delete,
}

#[derive(Debug)]
pub struct TerminalReader {
    tty: fs::File,
}

impl TerminalReader {
    pub fn new() -> io::Result<Self> {
        let tty = fs::File::open("/dev/tty")?;
        Ok(Self { tty })
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        let mut buf = [0];
        match self.tty.read(&mut buf).map_err(|v| v.to_string())? {
            0 => Err("unexpected eof".to_string()),
            _ => Ok(buf[0]),
        }
    }

    // ^[
    fn read_escape(&mut self) -> Result<TerminalEscape, String> {
        let next = self.read_u8()?;
        if next != b'[' {
            return Err(format!("unexpected: {:x}", next));
        };

        match self.read_u8()? {
            b'D' => return Ok(TerminalEscape::LeftArrow),
            b'C' => return Ok(TerminalEscape::RightArrow),
            _ => todo!(),
        }
    }

    pub fn read_input(&mut self) -> Result<Option<TerminalInput>, String> {
        let mut buf = [0];
        match self.tty.read(&mut buf).map_err(|v| v.to_string())? {
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
}

impl TerminalWriter {
    pub fn new() -> io::Result<Self> {
        let tty = fs::File::create("/dev/tty")?;
        let fd = tty.as_raw_fd();

        let mut tty = io::BufWriter::new(tty);
        switch_to_alternate_terminal(&mut tty)?;

        Ok(Self {
            tty,
            fd,
            original_termios: unsafe { enable_raw_mode(fd) },
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tty.flush()
    }

    fn newline_start(&mut self) -> io::Result<()> {
        self.write("\r\n".as_bytes())
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.tty.write_all(buf)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.write("\x1b[2J\x1b[H".as_bytes())
    }

    fn move_cursor(&mut self, line: usize, column: usize) -> io::Result<()> {
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
        switch_to_normal_terminal(&mut self.tty).unwrap();
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

fn switch_to_alternate_terminal<T: Write>(tty: &mut T) -> io::Result<()> {
    tty.write_all("\x1b[?1049h\x1b[2J\x1b[H".as_bytes())
}

fn switch_to_normal_terminal<T: Write>(tty: &mut T) -> io::Result<()> {
    tty.write_all("\x1b[2J\x1b[H\x1b[?1049l".as_bytes())
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
            left_lines: size.ws_col as usize,
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
    fn input(&mut self, input: &TerminalInput);
    fn render(&self) -> ComponentPromptOut;
}

pub trait ComponentData {
    fn render(&self) -> ComponentDataOut;
}

pub enum Component {
    Prompt(Box<dyn ComponentPrompt>),
    Data(Box<dyn ComponentData>),
}

enum TerminalRendererEvent {
    Resize,
    Input(TerminalInput),
    Redraw,
    Quit,
}

pub struct TerminalRenderer {
    components: Vec<Component>,
    size: libc::winsize,
    terminal_writer: TerminalWriter,

    size_update_handle: thread::JoinHandle<()>,
    input_handle: thread::JoinHandle<()>,
    redraw_event_handle: thread::JoinHandle<()>,

    event_rx: sync::mpsc::Receiver<TerminalRendererEvent>,
}

impl TerminalRenderer {
    pub fn new(components: Vec<Component>, redraw_rx: sync::mpsc::Receiver<()>) -> Self {
        let (event_tx, event_rx) = sync::mpsc::sync_channel(0);

        let redraw_event_handle = thread::spawn({
            let event_tx = event_tx.clone();
            move || {
                loop {
                    redraw_rx.recv().unwrap();
                    event_tx.send(TerminalRendererEvent::Redraw).unwrap();
                }
            }
        });

        let size_update_handle = thread::spawn({
            let event_tx = event_tx.clone();
            let mut signals = signal_hook::iterator::Signals::new(&[
                signal_hook::consts::SIGWINCH,
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
            ])
            .unwrap();

            move || {
                for signal in &mut signals {
                    match signal {
                        libc::SIGWINCH => {
                            event_tx.send(TerminalRendererEvent::Resize).unwrap();
                        }
                        libc::SIGINT | libc::SIGTERM => {
                            event_tx.send(TerminalRendererEvent::Quit).unwrap();
                        }
                        _ => unreachable!(),
                    }
                }
            }
        });

        let input_handle = thread::spawn({
            let event_tx = event_tx.clone();
            let mut terminal_reader = TerminalReader::new().unwrap();
            move || {
                loop {
                    let input = terminal_reader.read_input().unwrap();
                    if let Some(input) = input {
                        event_tx.send(TerminalRendererEvent::Input(input)).unwrap();
                    }
                }
            }
        });

        let terminal_writer = TerminalWriter::new().unwrap();
        let size = terminal_writer.size();

        Self {
            size,
            terminal_writer,
            components,
            size_update_handle,
            input_handle,
            redraw_event_handle,
            event_rx,
        }
    }

    fn handle_size(&mut self) {
        self.size = self.terminal_writer.size();
    }

    fn render_component_prompt(
        &mut self,
        out: ComponentPromptOut,
        state: &mut TerminalRenderState,
    ) {
        state.left_lines -= 1;

        let mut cols = self.size.ws_col as usize;
        let chevron = "> ".as_bytes();
        cols -= chevron.len();
        self.terminal_writer.write(chevron).unwrap();

        if out.query.len() <= cols {
            self.terminal_writer.write(out.query.as_bytes()).unwrap();
        } else {
            let offset = out.query.len() - cols;
            self.terminal_writer
                .write(out.query.as_str()[offset..].as_bytes())
                .unwrap();
        }

        state.cursor_line = 1;
        state.cursor_col = out.cursor_index + chevron.len() + 1;
    }

    fn render_component_data(&mut self, out: ComponentDataOut, state: &mut TerminalRenderState) {
        self.terminal_writer.newline_start().unwrap();
        state.left_lines -= 1;
        self.terminal_writer
            .write("-".repeat(self.size.ws_col as usize).as_bytes())
            .unwrap();

        let as_string = unsafe { String::from_utf8_unchecked(out.0) };

        for line in as_string.split("\n").take(state.left_lines as usize) {
            self.terminal_writer.newline_start().unwrap();
            self.terminal_writer.write(line.as_bytes()).unwrap();
        }
        state.left_lines = 0;
    }

    fn rerender(&mut self) {
        self.terminal_writer.clear().unwrap();

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
                ComponentRenderOut::Prompt(x) => self.render_component_prompt(x, &mut state),
                ComponentRenderOut::Data(x) => self.render_component_data(x, &mut state),
            }
        }

        self.terminal_writer
            .move_cursor(state.cursor_line, state.cursor_col)
            .unwrap();

        self.terminal_writer.flush().unwrap();
    }

    pub fn listen(mut self) {
        // todo: wait for handles
        // not only for local, but component as well
        loop {
            self.rerender();
            match self.event_rx.recv().unwrap() {
                TerminalRendererEvent::Resize => self.handle_size(),
                TerminalRendererEvent::Input(terminal_input) => {
                    if let TerminalInput::Ctrl(ch) = &terminal_input {
                        if *ch == b'c' {
                            todo!("quit");
                        }
                    }
                    for comp in &mut self.components {
                        match comp {
                            Component::Prompt(x) => x.input(&terminal_input),
                            _ => {}
                        }
                    }
                }
                TerminalRendererEvent::Redraw => {}
                TerminalRendererEvent::Quit => {
                    todo!("quit");
                }
            }
        }
    }
}
