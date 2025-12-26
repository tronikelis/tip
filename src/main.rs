use anyhow::{Context, Result, anyhow};
use std::{
    env,
    io::{self, Read, Write},
    process, sync, thread,
};

mod child;
mod terminal;

macro_rules! onerr {
    ($e:expr, $s:block) => {{
        match $e {
            Ok(v) => v,
            Err(_) => $s,
        }
    }};
}

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
                _ => {}
            },
            _ => {}
        }

        Ok(())
    }
}

fn create_command(
    cmd: &str,
    args: &[String],
    query: &str,
    input: &Option<sync::Arc<Vec<u8>>>,
) -> process::Command {
    let mut command = process::Command::new(cmd);
    command
        .args(args)
        .stdin(if input.is_some() {
            process::Stdio::piped()
        } else {
            process::Stdio::null()
        })
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped());

    if !query.is_empty() {
        command.arg(&query);
    }

    command
}

struct UiWaitingProcess {
    data: sync::Arc<sync::Mutex<Vec<u8>>>,
}

impl UiWaitingProcess {
    fn new(
        cmd: String,
        args: Vec<String>,
        input: Option<sync::Arc<Vec<u8>>>,
        redraw_tx: sync::mpsc::SyncSender<()>,
        query_rx: sync::mpsc::Receiver<String>,
    ) -> Self {
        let data = sync::Arc::new(sync::Mutex::new(Vec::new()));
        Self::start(cmd, args, input, redraw_tx, query_rx, data.clone());
        Self { data }
    }

    fn start(
        cmd: String,
        args: Vec<String>,
        input: Option<sync::Arc<Vec<u8>>>,
        redraw_tx: sync::mpsc::SyncSender<()>,
        query_rx: sync::mpsc::Receiver<String>,
        data: sync::Arc<sync::Mutex<Vec<u8>>>,
    ) -> thread::JoinHandle<()> {
        thread::spawn({
            move || {
                let mut _child: Option<_> = None;
                let mut query = String::new();
                loop {
                    let mut command = create_command(&cmd, &args, &query, &input);
                    _child = Some(child::DroppableChild::new(onerr!(command.spawn(), {
                        continue;
                    })));
                    let Some(child) = &mut _child else {
                        unreachable!();
                    };

                    let stdin = child.0.stdin.take();
                    let stdout = child.0.stdout.take().unwrap();
                    let stderr = child.0.stderr.take().unwrap();

                    onerr!(Self::reset_data(data.clone(), redraw_tx.clone()), {
                        return;
                    });

                    thread::spawn({
                        let input = input.clone();
                        let data = data.clone();
                        let redraw_tx = redraw_tx.clone();
                        move || {
                            let write_handle = input.map(|input| {
                                thread::spawn(move || {
                                    let _ = stdin.unwrap().write_all(&input);
                                })
                            });

                            let _ =
                                Self::read_child_stream(stdout, data.clone(), redraw_tx.clone());
                            let _ = Self::read_child_stream(stderr, data, redraw_tx);

                            if let Some(write_handle) = write_handle {
                                write_handle.join().unwrap();
                            }
                        }
                    });

                    query = onerr!(query_rx.recv(), { return });
                }
            }
        })
    }

    fn read_child_stream(
        mut stream: impl Read,
        data: sync::Arc<sync::Mutex<Vec<u8>>>,
        redraw_tx: sync::mpsc::SyncSender<()>,
    ) -> Result<()> {
        let mut has_read = false;
        loop {
            let mut buf = [0; 1 << 13];
            let size = stream.read(&mut buf)?;
            if size == 0 {
                break;
            }
            if !has_read {
                has_read = true;
                Self::reset_data(data.clone(), redraw_tx.clone())?;
            }
            Self::push_to_data(data.clone(), &buf[..size], redraw_tx.clone())?
        }

        Ok(())
    }

    fn reset_data(
        data: sync::Arc<sync::Mutex<Vec<u8>>>,
        redraw_tx: sync::mpsc::SyncSender<()>,
    ) -> Result<()> {
        *data.lock().unwrap() = Vec::new();
        redraw_tx.send(())?;
        Ok(())
    }

    fn push_to_data(
        data: sync::Arc<sync::Mutex<Vec<u8>>>,
        buf: &[u8],
        redraw_tx: sync::mpsc::SyncSender<()>,
    ) -> Result<()> {
        {
            let mut data = data.lock().unwrap();
            buf.iter().for_each(|v| data.push(*v));
            // mutex gets dropped here
        }
        redraw_tx.send(())?;
        Ok(())
    }
}

impl terminal::ComponentData for UiWaitingProcess {
    fn render(&self) -> terminal::ComponentDataOut {
        let data = self.data.lock().unwrap().clone();
        terminal::ComponentDataOut(data)
    }
}

fn pipe_cmd_stdout(
    cmd: &str,
    args: &[String],
    query: &str,
    input: Option<sync::Arc<Vec<u8>>>,
) -> Result<()> {
    let mut command = create_command(cmd, args, query, &input);
    let mut child = command.spawn()?;

    let stdin_handle = input.map(|input| {
        thread::spawn({
            let mut stdin = child.stdin.take().unwrap();
            move || {
                let _ = stdin.write_all(&input);
            }
        })
    });

    io::copy(&mut child.stdout.take().unwrap(), &mut io::stdout())?;

    if let Some(stdin_handle) = stdin_handle {
        stdin_handle.join().unwrap();
    }

    Ok(())
}

fn main_err() -> Result<()> {
    let stdin_input = {
        let mut stdin_input = None;
        if !terminal::isatty(libc::STDIN_FILENO) {
            let mut v = Vec::new();
            io::stdin()
                .read_to_end(&mut v)
                .with_context(|| "failed reading stdin")?;
            stdin_input = Some(sync::Arc::new(v));
        }
        stdin_input
    };

    let Some(cmd) = env::args().skip(1).next() else {
        return Err(anyhow!("expected first argument to be command".to_string()));
    };
    let cmd_args = env::args().skip(2).collect::<Vec<_>>();

    // todo: figure out how to do this sync
    // there is a deadlock between query_rx, query_tx, redraw_tx
    let (query_tx, query_rx) = sync::mpsc::channel();
    let (redraw_tx, redraw_rx) = sync::mpsc::sync_channel(0);

    let mut ui_waiting_process = UiWaitingProcess::new(
        cmd.clone(),
        cmd_args.clone(),
        stdin_input.clone(),
        redraw_tx.clone(),
        query_rx,
    );
    let mut ui_prompt = UiPrompt::new(query_tx);
    let mut print_to_stdout = false;

    terminal::TerminalRenderer::new(
        vec![
            terminal::Component::Prompt(&mut ui_prompt),
            terminal::Component::Data(&mut ui_waiting_process),
        ],
        redraw_rx,
    )?
    .start(|input| match input {
        terminal::TerminalInput::Ctrl(ch) => match ch {
            // enter
            b'm' => {
                print_to_stdout = true;
                true
            }
            // c-c
            b'c' => true,
            _ => false,
        },
        terminal::TerminalInput::Escape(esc) => match esc {
            terminal::TerminalEscape::Timeout => true,
            _ => false,
        },
        _ => false,
    })?;

    if print_to_stdout {
        pipe_cmd_stdout(&cmd, &cmd_args, &ui_prompt.get_string(), stdin_input)?;
    }

    Ok(())
}

fn main() {
    if let Err(err) = main_err() {
        eprintln!("{}", err);
        process::exit(1);
    }
}
