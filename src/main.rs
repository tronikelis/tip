use anyhow::{Context, Result, anyhow};
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

    fn add_character(&mut self, ch: char) -> Result<()> {
        self.query.insert(self.cursor_index, ch);
        self.cursor_index += 1;

        self.tx.send(self.get_string())?;
        Ok(())
    }

    fn delete_character(&mut self) -> Result<()> {
        if self.cursor_index == 0 {
            return Ok(());
        }

        self.query.remove(self.cursor_index - 1);
        self.cursor_index -= 1;

        self.tx.send(self.get_string())?;
        Ok(())
    }
}

impl terminal::ComponentPrompt for UiPrompt {
    fn render(&self) -> terminal::ComponentPromptOut {
        terminal::ComponentPromptOut {
            cursor_index: self.cursor_index,
            query: self.get_string(),
        }
    }

    fn input(&mut self, input: &terminal::TerminalInput) -> Result<()> {
        match input {
            terminal::TerminalInput::Delete => {
                self.delete_character()?;
            }
            terminal::TerminalInput::Printable(ch) => {
                self.add_character(*ch as char)?;
            }
            terminal::TerminalInput::Escape(escape) => match escape {
                terminal::TerminalEscape::LeftArrow => self.move_cursor(-1),
                terminal::TerminalEscape::RightArrow => self.move_cursor(1),
            },
            _ => {}
        }

        Ok(())
    }
}

struct UiWaitingProcess {
    stdout: sync::Arc<sync::Mutex<Vec<u8>>>,
}

impl UiWaitingProcess {
    fn new(
        cmd: String,
        args: Vec<String>,
        input: Vec<u8>,
        redraw_tx: sync::mpsc::SyncSender<()>,
        query_rx: sync::mpsc::Receiver<String>,
    ) -> (Self, thread::JoinHandle<()>) {
        let stdout = sync::Arc::new(sync::Mutex::new(Vec::new()));
        let handle = thread::spawn({
            let stdout = stdout.clone();
            move || {
                let mut child_handle: Option<(u32, thread::JoinHandle<()>)> = None;
                loop {
                    let Ok(query) = query_rx.recv() else {
                        if let Some(child_handle) = child_handle {
                            Self::kill_child(child_handle.0 as i32, child_handle.1);
                        }

                        break;
                    };

                    if let Some(child_handle) = child_handle {
                        Self::kill_child(child_handle.0 as i32, child_handle.1);
                    }

                    let mut child = match process::Command::new(cmd.clone())
                        .args(args.clone().iter())
                        .arg(query)
                        .stdin(process::Stdio::piped())
                        .stdout(process::Stdio::piped())
                        .stderr(process::Stdio::piped())
                        .spawn()
                    {
                        Ok(v) => v,
                        Err(err) => {
                            *stdout.lock().unwrap() = err.to_string().as_bytes().to_vec();
                            let _ = redraw_tx.send(());
                            break;
                        }
                    };

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
                                let _ = write_stdin.join().unwrap(); // ignore write error

                                let Ok(_) = child.wait() else { return };
                                if let Ok(child_stdout) = child_stdout {
                                    *stdout.lock().unwrap() = child_stdout;
                                    let _ = redraw_tx.send(());
                                }
                            }
                        }),
                    ));
                }
            }
        });

        (Self { stdout }, handle)
    }

    fn kill_child(id: i32, handle: thread::JoinHandle<()>) {
        unsafe {
            libc::kill(id as i32, 9);
        }
        handle.join().unwrap();
    }
}

impl terminal::ComponentData for UiWaitingProcess {
    fn render(&self) -> terminal::ComponentDataOut {
        let stdout = self.stdout.lock().unwrap().clone();
        terminal::ComponentDataOut(stdout)
    }
}

fn read_to_end<T: Read>(reader: T) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    io::BufReader::new(reader).read_to_end(&mut buf)?;
    Ok(buf)
}

fn main_err() -> Result<()> {
    if terminal::isatty(libc::STDIN_FILENO) {
        return Err(anyhow!("stdin is a terminal, aborting".to_string()));
    }

    let stdin_input = read_to_end(io::stdin()).with_context(|| "failed reading stdin")?;

    let (query_tx, query_rx) = sync::mpsc::channel(); // todo: figure out how to do this sync
    let (redraw_tx, redraw_rx) = sync::mpsc::sync_channel(0);

    let Some(cmd) = env::args().skip(1).next() else {
        return Err(anyhow!("expected first argument to be command".to_string()));
    };

    let cmd_args = env::args().skip(2).collect();

    let (ui_waiting_process_component, ui_waiting_process_handle) =
        UiWaitingProcess::new(cmd, cmd_args, stdin_input, redraw_tx.clone(), query_rx);

    terminal::TerminalRenderer::new(
        vec![
            terminal::Component::Prompt(Box::new(UiPrompt::new(query_tx))),
            terminal::Component::Data(Box::new(ui_waiting_process_component)),
        ],
        redraw_rx,
    )?
    .start()?;

    ui_waiting_process_handle.join().unwrap();
    Ok(())
}

fn main() {
    if let Err(err) = main_err() {
        eprintln!("{}", err);
        process::exit(1);
    }
}
