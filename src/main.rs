use std::{
    env, fs,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    process, sync, thread,
};

mod terminal;

#[derive(Debug)]
struct UiQuery {
    cursor_index: usize,
    text: Vec<char>,
    tx: sync::mpsc::Sender<String>,
}

impl UiQuery {
    fn new(tx: sync::mpsc::Sender<String>) -> Self {
        Self {
            tx,
            cursor_index: 0,
            text: Vec::new(),
        }
    }

    fn move_cursor(&mut self, columns: isize) {
        let mut cursor_index = self.cursor_index as isize;
        cursor_index += columns;
        self.cursor_index = cursor_index.max(0).min(self.text.len() as isize) as usize;
    }

    fn get_string(&self) -> String {
        self.text.iter().collect()
    }

    fn add_character(&mut self, ch: char) {
        self.text.insert(self.cursor_index, ch);
        self.cursor_index += 1;

        self.tx.send(self.get_string()).unwrap();
    }

    fn delete_character(&mut self) {
        if self.cursor_index == 0 {
            return;
        }

        self.text.remove(self.cursor_index - 1);
        self.cursor_index -= 1;

        self.tx.send(self.get_string()).unwrap();
    }
}

struct UiState {
    query: UiQuery,
    process_stdout: Option<Vec<u8>>,
    terminal: terminal::TerminalWriter,
    event_rx: sync::mpsc::Receiver<UiEvent>,
    event_tx: sync::mpsc::SyncSender<UiEvent>,
}

impl UiState {
    fn new(
        query_tx: sync::mpsc::Sender<String>,
        event_tx: sync::mpsc::SyncSender<UiEvent>,
        event_rx: sync::mpsc::Receiver<UiEvent>,
    ) -> Result<Self, String> {
        let terminal = terminal::TerminalWriter::new().map_err(|v| v.to_string())?;
        terminal.enable_raw_mode();
        Ok(Self {
            query: UiQuery::new(query_tx),
            process_stdout: None,
            terminal,
            event_rx,
            event_tx,
        })
    }

    fn redraw(&mut self) -> Result<(), String> {
        self.terminal.clear().map_err(|v| v.to_string())?;

        self.terminal
            .write_all(self.query.get_string().as_bytes())
            .map_err(|v| v.to_string())?;

        if let Some(process_stdout) = &self.process_stdout {
            self.terminal
                .write_all("\n".as_bytes())
                .map_err(|v| v.to_string())?;

            self.terminal
                .write_all(process_stdout)
                .map_err(|v| v.to_string())?;
        }

        self.terminal
            .move_cursor(1, self.query.cursor_index + 1)
            .map_err(|v| v.to_string())?;

        Ok(())
    }

    fn handle_input_printable(&mut self, ch: u8) {
        self.query.add_character(ch as char);
    }

    fn handle_input_delete(&mut self) {
        self.query.delete_character();
    }

    fn handle_input_escape(&mut self, escape: terminal::TerminalEscape) {
        match escape {
            terminal::TerminalEscape::LeftArrow => {
                self.query.move_cursor(-1);
            }
            terminal::TerminalEscape::RightArrow => {
                self.query.move_cursor(1);
            }
        }
    }

    fn handle_terminal_input(&mut self, input: terminal::TerminalInput) {
        match input {
            terminal::TerminalInput::Printable(ch) => self.handle_input_printable(ch),
            terminal::TerminalInput::Delete => self.handle_input_delete(),
            terminal::TerminalInput::Escape(escape) => self.handle_input_escape(escape),
            _ => todo!(),
        };
    }

    fn start_input_handle(&self) -> Result<thread::JoinHandle<()>, String> {
        let mut terminal = terminal::TerminalReader::new().map_err(|v| v.to_string())?;
        Ok(thread::spawn({
            let event_tx = self.event_tx.clone();
            move || {
                loop {
                    let input = terminal.read_input().unwrap().unwrap();
                    event_tx.send(UiEvent::TerminalInput(input)).unwrap();
                }
            }
        }))
    }

    fn start(mut self) -> Result<(), String> {
        let input_handle = self.start_input_handle()?;

        loop {
            self.redraw()?;
            match self.event_rx.recv().map_err(|v| v.to_string()) {
                Ok(event) => match event {
                    UiEvent::SetStdout(stdout) => {
                        self.process_stdout = Some(stdout);
                    }
                    UiEvent::TerminalInput(input) => {
                        self.handle_terminal_input(input);
                    }
                },
                Err(err) => {
                    input_handle.join().unwrap();
                    return Err(err.to_string());
                }
            }
        }
    }
}

fn launch_waiting_process(
    cmd: String,
    args: Vec<String>,
    input: Vec<u8>,
    query_rx: sync::mpsc::Receiver<String>,
    ui_event_tx: sync::mpsc::SyncSender<UiEvent>,
) -> thread::JoinHandle<()> {
    let input = sync::Arc::new(input);
    let args = sync::Arc::new(args);
    thread::spawn(move || {
        let mut child_handle: Option<(u32, thread::JoinHandle<()>)> = None;
        loop {
            let query = query_rx.recv().unwrap();
            if let Some(child_handle) = child_handle {
                unsafe {
                    libc::kill(child_handle.0 as i32, 9);
                }
                child_handle.1.join().unwrap();
            }

            let mut child = process::Command::new(cmd.clone())
                .args(args.clone().iter())
                .arg(query)
                .stdin(process::Stdio::piped())
                .stdout(process::Stdio::piped())
                .stderr(process::Stdio::piped())
                .spawn()
                .unwrap();

            child_handle = Some((
                child.id(),
                thread::spawn({
                    let input = input.clone();
                    let ui_event_tx = ui_event_tx.clone();
                    move || {
                        let mut child_stdin = child.stdin.take().unwrap();
                        let write_stdin = thread::spawn(move || child_stdin.write_all(&input));

                        let child_stdout = read_to_end(child.stdout.take().unwrap());
                        let _ = write_stdin.join();

                        child.wait().unwrap();
                        if let Ok(child_stdout) = child_stdout {
                            ui_event_tx.send(UiEvent::SetStdout(child_stdout)).unwrap();
                        }
                    }
                }),
            ));
        }
    })
}

fn read_to_end<T: Read>(reader: T) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    io::BufReader::new(reader).read_to_end(&mut buf)?;
    Ok(buf)
}

enum UiEvent {
    SetStdout(Vec<u8>),
    TerminalInput(terminal::TerminalInput),
}

fn main() {
    if terminal::isatty(libc::STDIN_FILENO) {
        eprintln!("stdin is a terminal, aborting");
        process::exit(1);
    }

    let (query_tx, query_rx) = sync::mpsc::channel::<String>();
    let (ui_event_tx, ui_event_rx) = sync::mpsc::sync_channel::<UiEvent>(0);

    let stdin_input = read_to_end(io::stdin()).unwrap();
    let waiting_process_handle = launch_waiting_process(
        env::args().skip(1).next().unwrap(),
        env::args().skip(2).collect(),
        stdin_input,
        query_rx,
        ui_event_tx.clone(),
    );

    UiState::new(query_tx, ui_event_tx, ui_event_rx)
        .unwrap()
        .start()
        .unwrap();

    waiting_process_handle.join().unwrap();
}
