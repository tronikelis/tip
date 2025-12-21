use std::{
    fs,
    io::{self, Read, Write},
    mem,
    os::fd::AsRawFd,
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
    tty: fs::File,
}

impl TerminalWriter {
    pub fn new() -> io::Result<Self> {
        let tty = fs::File::create("/dev/tty")?;
        Ok(Self { tty })
    }

    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.tty.write_all(buf)
    }

    pub fn clear(&mut self) -> io::Result<()> {
        self.write_all("\x1b[2J\x1b[H".as_bytes())
    }

    pub fn move_cursor_to_column(&mut self, column: usize) -> io::Result<()> {
        self.write_all(format!("\x1b[{}G", column).as_bytes())
    }

    pub fn enable_raw_mode(&self) {
        enable_raw_mode(self.tty.as_raw_fd());
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
