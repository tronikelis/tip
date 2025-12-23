use std::{
    env,
    io::{self, Read, Write},
    process, sync, thread,
};

mod terminal;

#[derive(Debug)]
struct UiPrompt {
    cursor_index: usize,
    query: Vec<char>,
    tx: sync::mpsc::Sender<String>,
}

impl UiPrompt {
    fn new(tx: sync::mpsc::Sender<String>) -> Self {
        Self {
            cursor_index: 0,
            query: Vec::new(),
            tx,
        }
    }

    fn get_string(&self) -> String {
        self.query.iter().collect()
    }

    fn move_cursor(&mut self, columns: isize) {
        let mut cursor_index = self.cursor_index as isize;
        cursor_index += columns;
        self.cursor_index = cursor_index.max(0).min(self.query.len() as isize) as usize;
    }

    fn add_character(&mut self, ch: char) {
        self.query.insert(self.cursor_index, ch);
        self.cursor_index += 1;

        self.tx.send(self.get_string()).unwrap();
    }

    fn delete_character(&mut self) {
        if self.cursor_index == 0 {
            return;
        }

        self.query.remove(self.cursor_index - 1);
        self.cursor_index -= 1;

        self.tx.send(self.get_string()).unwrap();
    }
}

impl terminal::ComponentPrompt for UiPrompt {
    fn render(&self) -> terminal::ComponentPromptOut {
        terminal::ComponentPromptOut {
            cursor_index: self.cursor_index,
            query: self.get_string(),
        }
    }

    fn input(&mut self, input: &terminal::TerminalInput) {
        match input {
            terminal::TerminalInput::Delete => self.delete_character(),
            terminal::TerminalInput::Printable(ch) => self.add_character(*ch as char),
            terminal::TerminalInput::Escape(escape) => match escape {
                terminal::TerminalEscape::LeftArrow => self.move_cursor(-1),
                terminal::TerminalEscape::RightArrow => self.move_cursor(1),
            },
            _ => {}
        }
    }
}

struct UiWaitingProcess {
    handle: thread::JoinHandle<()>,
    stdout: sync::Arc<sync::Mutex<Vec<u8>>>,
}

impl UiWaitingProcess {
    fn new(
        cmd: String,
        args: Vec<String>,
        input: Vec<u8>,
        redraw_tx: sync::mpsc::SyncSender<()>,
        query_rx: sync::mpsc::Receiver<String>,
    ) -> Self {
        let stdout = sync::Arc::new(sync::Mutex::new(Vec::new()));
        let handle = thread::spawn({
            let stdout = stdout.clone();
            move || {
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
                            let redraw_tx = redraw_tx.clone();
                            let stdout = stdout.clone();
                            move || {
                                let mut child_stdin = child.stdin.take().unwrap();
                                let write_stdin =
                                    thread::spawn(move || child_stdin.write_all(&input));

                                let child_stdout = read_to_end(child.stdout.take().unwrap());
                                let _ = write_stdin.join();

                                child.wait().unwrap();
                                if let Ok(child_stdout) = child_stdout {
                                    *stdout.lock().unwrap() = child_stdout;
                                    redraw_tx.send(()).unwrap();
                                }
                            }
                        }),
                    ));
                }
            }
        });

        Self { handle, stdout }
    }

    // todo: this isn't called anywhere
    fn wait(self) {
        self.handle.join().unwrap()
    }
}

impl terminal::ComponentStream for UiWaitingProcess {
    fn render(&self) -> terminal::ComponentStreamOut {
        let stdout = self.stdout.lock().unwrap().clone();
        terminal::ComponentStreamOut(stdout)
    }
}

fn read_to_end<T: Read>(reader: T) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    io::BufReader::new(reader).read_to_end(&mut buf)?;
    Ok(buf)
}

fn main() {
    if terminal::isatty(libc::STDIN_FILENO) {
        eprintln!("stdin is a terminal, aborting");
        process::exit(1);
    }
    let stdin_input = read_to_end(io::stdin()).unwrap();

    let (query_tx, query_rx) = sync::mpsc::channel(); // todo: figure out how to do this sync
    let (redraw_tx, redraw_rx) = sync::mpsc::sync_channel(0);

    terminal::TerminalRenderer::new(
        vec![
            terminal::Component::Prompt(Box::new(UiPrompt::new(query_tx))),
            terminal::Component::Stream(Box::new(UiWaitingProcess::new(
                env::args().skip(1).next().unwrap(),
                env::args().skip(2).collect(),
                stdin_input,
                redraw_tx.clone(),
                query_rx,
            ))),
        ],
        redraw_rx,
    )
    .listen();
}
