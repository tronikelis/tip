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
}

impl TerminalWriter {
    pub fn new() -> io::Result<Self> {
        let tty = fs::File::create("/dev/tty")?;
        let fd = tty.as_raw_fd();
        let tty = io::BufWriter::new(tty);
        Ok(Self { tty, fd })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tty.flush()
    }

    fn write_newline(&mut self) -> io::Result<()> {
        self.write("\n".as_bytes())
    }

    pub fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.tty.write_all(buf)
    }

    pub fn clear(&mut self) -> io::Result<()> {
        self.write("\x1b[2J\x1b[H".as_bytes())
    }

    pub fn move_cursor_to_column(&mut self, column: usize) -> io::Result<()> {
        self.write(format!("\x1b[{}G", column).as_bytes())
    }

    pub fn move_cursor(&mut self, line: usize, column: usize) -> io::Result<()> {
        self.write(format!("\x1b[{};{}H", line, column).as_bytes())
    }

    pub fn enable_raw_mode(&self) {
        enable_raw_mode(self.fd);
    }

    pub fn size(&self) -> libc::winsize {
        get_terminal_size(self.fd)
    }
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

pub fn enable_raw_mode(tty_fd: i32) {
    let mut termios = mem::MaybeUninit::<libc::termios>::uninit();
    unsafe { libc::tcgetattr(tty_fd, termios.as_mut_ptr()) };
    let mut termios = unsafe { termios.assume_init() };
    termios.c_lflag &= !(libc::ECHO | libc::ICANON);

    unsafe {
        libc::tcsetattr(tty_fd, libc::TCSAFLUSH, &termios);
    }
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
    Stream(ComponentStreamOut),
}

pub struct ComponentStreamOut(pub Vec<u8>);

pub struct ComponentPromptOut {
    pub query: String,
    pub cursor_index: usize,
}

pub trait ComponentPrompt {
    fn input(&mut self, input: &TerminalInput);
    fn render(&self) -> ComponentPromptOut;
}

pub trait ComponentStream {
    fn render(&self) -> ComponentStreamOut;
}

pub enum Component {
    Prompt(Box<dyn ComponentPrompt>),
    Stream(Box<dyn ComponentStream>),
}

pub enum TerminalRendererEvent {
    Resize,
    Input(TerminalInput),
    Redraw,
}

pub struct TerminalRenderer {
    components: Vec<Component>,
    size: libc::winsize,
    terminal_writer: TerminalWriter,

    size_update_handle: thread::JoinHandle<()>,
    input_handle: thread::JoinHandle<()>,

    event_rx: sync::mpsc::Receiver<TerminalRendererEvent>,
}

impl TerminalRenderer {
    pub fn new(
        components: Vec<Component>,
        event_tx: sync::mpsc::SyncSender<TerminalRendererEvent>,
        event_rx: sync::mpsc::Receiver<TerminalRendererEvent>,
    ) -> Self {
        let (event_tx, event_rx) = sync::mpsc::sync_channel(0);

        let size_update_handle = thread::spawn({
            let event_tx = event_tx.clone();
            let mut signals =
                signal_hook::iterator::Signals::new(&[signal_hook::consts::SIGWINCH]).unwrap();

            move || {
                for _ in &mut signals {
                    event_tx.send(TerminalRendererEvent::Resize).unwrap();
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
        terminal_writer.enable_raw_mode();
        let size = terminal_writer.size();

        Self {
            size,
            terminal_writer,
            components,
            size_update_handle,
            input_handle,
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

        state.cursor_col = out.cursor_index + chevron.len() + 1;
        state.cursor_line = 1;
    }

    fn render_component_stream(
        &mut self,
        out: ComponentStreamOut,
        state: &mut TerminalRenderState,
    ) {
        self.terminal_writer.write_newline().unwrap();
        state.left_lines -= 1;
        self.terminal_writer
            .write("-".repeat(self.size.ws_col as usize).as_bytes())
            .unwrap();

        let as_string = unsafe { String::from_utf8_unchecked(out.0) };

        self.terminal_writer.write_newline().unwrap();
        for line in as_string.split("\n").take(state.left_lines as usize) {
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
                Component::Stream(x) => ComponentRenderOut::Stream(x.render()),
            })
            .collect::<Vec<_>>();

        let mut state = TerminalRenderState::new(&self.size);
        for x in rendered {
            match x {
                ComponentRenderOut::Prompt(x) => self.render_component_prompt(x, &mut state),
                ComponentRenderOut::Stream(x) => self.render_component_stream(x, &mut state),
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
                    for comp in &mut self.components {
                        match comp {
                            Component::Prompt(x) => x.input(&terminal_input),
                            _ => {}
                        }
                    }
                }
                TerminalRendererEvent::Redraw => {}
            }
        }
    }
}
