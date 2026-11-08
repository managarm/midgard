//! A session: one connection from the CLI, usually running one command.

use std::cell::Cell;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::rc::Rc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use async_io::{Async, Timer};
use futures_lite::{AsyncReadExt, AsyncWriteExt, future};
use midgard_protocol::nonblocking::{read_message, write_message};
use midgard_protocol::session::{Exec, Request, Response, WindowSize};

use super::process::{self, Stdio};
use super::sys::{self, Error};
use super::{PATH, log, ready, reaper, rt};

/// How long we wait for output after the command exited (e.g., for background
/// processes that keep the terminal open, we do not wait forever).
const OUTPUT_GRACE: Duration = Duration::from_millis(500);

/// The session's connection (a vsock stream socket). Requests are read by one task;
/// responses are written by one task (see [`write_responses`]), so frames do not mix.
type Stream = Rc<Async<File>>;

pub async fn handle(stream: Async<File>) {
    let stream = Rc::new(stream);
    let reply = |response: Response| {
        let stream = stream.clone();
        async move {
            let _ = write_message(&mut &*stream, &response).await;
        }
    };
    match read_message::<Request>(&mut &*stream).await {
        Ok(Some(Request::Ping)) => reply(Response::Pong).await,
        Ok(Some(Request::Shutdown)) => {
            // Acknowledge first; the client then waits for the VM to stop.
            reply(Response::Exited {
                code: Some(0),
                signal: None,
            })
            .await;
            shutdown().await
        }
        Ok(Some(Request::Exec(exec))) => {
            ready::wait().await;
            if let Err(error) = run(exec, &stream).await {
                reply(Response::Error(error.to_string())).await;
            }
        }
        Ok(Some(_)) => reply(Response::Error("a session starts with Exec".into())).await,
        Ok(None) => {}
        Err(error) => log!("bad request: {error}"),
    }
}

/// root's home and login shell from /etc/passwd.
fn root_user() -> (String, String) {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.len() >= 7 && fields[2] == "0")
        .map(|fields| (fields[5].to_string(), fields[6].to_string()))
        .unwrap_or_else(|| ("/root".into(), "/bin/sh".into()))
}

fn environment(exec: &Exec, home: &str, shell: &str) -> Vec<String> {
    let mut env: Vec<(String, String)> = vec![
        ("PATH".into(), PATH.into()),
        ("HOME".into(), home.into()),
        ("USER".into(), "root".into()),
        ("LOGNAME".into(), "root".into()),
        ("SHELL".into(), shell.into()),
    ];
    if exec.tty.is_some() {
        env.push(("TERM".into(), "linux".into()));
    }
    for (name, value) in &exec.env {
        env.retain(|(n, _)| n != name);
        env.push((name.clone(), value.clone()));
    }
    env.into_iter().map(|(n, v)| format!("{n}={v}")).collect()
}

fn set_window_size(fd: RawFd, size: WindowSize) {
    let winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads a struct winsize.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize) };
}

/// Where the session's input goes.
enum Input {
    Pty(Rc<Async<File>>),
    Pipe(Option<Async<File>>),
}

fn pipe() -> Result<(File, File), Error> {
    let mut fds = [0; 2];
    // SAFETY: pipe2() writes two file descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(Error::last("pipe2()"));
    }
    // SAFETY: We own the new file descriptors.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

/// Writes the responses of a session until all senders are gone (or the client is).
async fn write_responses(stream: Stream, responses: Receiver<Response>) {
    while let Ok(response) = responses.recv().await {
        if write_message(&mut &*stream, &response).await.is_err() {
            return;
        }
    }
}

/// Forwards output from `source` until it ends.
async fn pump_output(source: Rc<Async<File>>, stderr: bool, responses: Sender<Response>) {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut reader = &*source;
    // A pty master returns EIO once no process has the slave open.
    while let Ok(count) = reader.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        let data = buffer[..count].to_vec();
        let response = if stderr {
            Response::Stderr(data)
        } else {
            Response::Stdout(data)
        };
        if responses.send(response).await.is_err() {
            break;
        }
    }
}

fn nonblocking(file: File) -> Result<Rc<Async<File>>, Error> {
    Async::new(file)
        .map(Rc::new)
        .map_err(|error| Error::io("registering a file descriptor", error))
}

async fn run(exec: Exec, stream: &Stream) -> Result<(), Error> {
    let (home, shell) = root_user();
    let env = environment(&exec, &home, &shell);
    let path_env = env
        .iter()
        .find_map(|e| e.strip_prefix("PATH="))
        .unwrap_or(PATH)
        .to_string();
    let (path, argv) = if exec.argv.is_empty() {
        // A login shell: argv[0] starts with a dash.
        let name = shell.rsplit('/').next().unwrap_or("sh");
        (shell.clone(), vec![format!("-{name}")])
    } else {
        let path = process::resolve(&exec.argv[0], &path_env)
            .ok_or_else(|| Error::Message(format!("{}: command not found", exec.argv[0])))?;
        (path, exec.argv.clone())
    };
    let cwd = exec
        .cwd
        .clone()
        .filter(|cwd| std::path::Path::new(cwd).is_dir())
        .unwrap_or(home);

    // The command's input and its outputs (with whether each one is stderr).
    let (pid, input, outputs) = match exec.tty {
        Some(size) => {
            let (mut master, mut slave) = (0, 0);
            // SAFETY: openpty() writes two file descriptors.
            if unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            } != 0
            {
                return Err(Error::last("openpty()"));
            }
            // SAFETY: We own the new file descriptors.
            let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
            // SAFETY: Setting FD_CLOEXEC on our own descriptors.
            unsafe {
                libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            }
            set_window_size(master.as_raw_fd(), size);
            let pid = process::spawn(&path, &argv, &env, &cwd, &Stdio::Pty(slave.as_raw_fd()))?;
            drop(slave);
            let master = nonblocking(master)?;
            (pid, Input::Pty(master.clone()), vec![(master, false)])
        }
        None => {
            let (stdin_read, stdin_write) = pipe()?;
            let (stdout_read, stdout_write) = pipe()?;
            let (stderr_read, stderr_write) = pipe()?;
            let stdio = Stdio::Pipes {
                stdin: stdin_read.as_raw_fd(),
                stdout: stdout_write.as_raw_fd(),
                stderr: stderr_write.as_raw_fd(),
            };
            let pid = process::spawn(&path, &argv, &env, &cwd, &stdio)?;
            drop((stdin_read, stdout_write, stderr_write));
            let stdin = Async::new(stdin_write)
                .map_err(|error| Error::io("registering a file descriptor", error))?;
            let outputs = vec![
                (nonblocking(stdout_read)?, false),
                (nonblocking(stderr_read)?, true),
            ];
            (pid, Input::Pipe(Some(stdin)), outputs)
        }
    };
    let exited = reaper::watch(pid);
    let (responses, to_client) = async_channel::bounded(8);
    let writer = rt::spawn(write_responses(stream.clone(), to_client));
    let _ = responses.send(Response::Started { pid: pid as u32 }).await;
    let pumps: Vec<_> = outputs
        .into_iter()
        .map(|(source, stderr)| rt::spawn(pump_output(source, stderr, responses.clone())))
        .collect();
    let finished = Rc::new(Cell::new(false));
    let input = rt::spawn(forward_input(stream.clone(), input, pid, finished.clone()));

    let status = exited
        .recv()
        .await
        .map_err(|_| Error::Message("lost the exit status".into()))?;
    finished.set(true);
    // Dropping the pumps after the grace period cancels them.
    future::or(
        async {
            for pump in pumps {
                pump.await;
            }
        },
        async {
            Timer::after(OUTPUT_GRACE).await;
        },
    )
    .await;
    let (code, signal) = process::exit_status(status);
    let _ = responses.send(Response::Exited { code, signal }).await;
    drop(responses);
    writer.await;
    // The session is over; stop reading requests.
    drop(input);
    Ok(())
}

/// Forwards requests to the command until the session ends.
async fn forward_input(stream: Stream, mut input: Input, pid: i32, finished: Rc<Cell<bool>>) {
    // The process group that receives signals: the terminal's foreground group if there
    // is a terminal, otherwise the session (the command is its leader).
    let group = |input: &Input| match input {
        // SAFETY: tcgetpgrp() only takes a file descriptor.
        Input::Pty(master) => match unsafe { libc::tcgetpgrp(master.as_raw_fd()) } {
            pgrp if pgrp > 0 => pgrp,
            _ => pid,
        },
        Input::Pipe(_) => pid,
    };
    loop {
        match read_message::<Request>(&mut &*stream).await {
            Ok(Some(Request::Stdin(data))) => {
                let result = match &input {
                    Input::Pty(master) => (&**master).write_all(&data).await,
                    Input::Pipe(Some(stdin)) => (&*stdin).write_all(&data).await,
                    Input::Pipe(None) => Ok(()),
                };
                if result.is_err() {
                    // The command does not read anymore; drop further input.
                    if let Input::Pipe(stdin) = &mut input {
                        *stdin = None;
                    }
                }
            }
            Ok(Some(Request::StdinEof)) => match &mut input {
                Input::Pipe(stdin) => *stdin = None,
                // On a terminal, EOF is the VEOF character (Ctrl-D).
                Input::Pty(master) => {
                    let _ = (&**master).write_all(&[4]).await;
                }
            },
            Ok(Some(Request::Resize(size))) => {
                if let Input::Pty(master) = &input {
                    set_window_size(master.as_raw_fd(), size);
                }
            }
            Ok(Some(Request::Signal(signal))) => {
                // SAFETY: kill() only takes numbers.
                unsafe { libc::kill(-group(&input), signal) };
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                // The client went away: hang up, like a closed terminal.
                if !finished.get() {
                    // SAFETY: kill() only takes numbers.
                    unsafe { libc::kill(-group(&input), libc::SIGHUP) };
                }
                return;
            }
        }
    }
}

/// How long the distribution's init may take to power off.
const POWEROFF_TIMEOUT: Duration = Duration::from_secs(30);

/// Stops all processes, unmounts everything and powers off.
async fn shutdown() {
    if super::init_runs() {
        // The distribution's init stops its services and powers off (and kills us on the
        // way). If it does not within the timeout, we do it ourselves.
        log!("asking init to power off");
        let path = process::resolve("poweroff", PATH).unwrap_or_else(|| "/sbin/poweroff".into());
        let argv = vec!["poweroff".to_string()];
        let env = vec![format!("PATH={PATH}")];
        match process::spawn_background(&path, &argv, &env) {
            Ok(_) => {
                Timer::after(POWEROFF_TIMEOUT).await;
                log!(
                    "init did not power off within {} s",
                    POWEROFF_TIMEOUT.as_secs()
                );
            }
            Err(error) => log!("running poweroff: {error}"),
        }
    }
    log!("shutting down");
    // SAFETY: kill(-1) signals all processes except us.
    unsafe { libc::kill(-1, libc::SIGTERM) };
    Timer::after(Duration::from_secs(1)).await;
    // SAFETY: See above.
    unsafe { libc::kill(-1, libc::SIGKILL) };
    Timer::after(Duration::from_millis(100)).await;
    // SAFETY: sync() has no preconditions.
    unsafe { libc::sync() };
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let targets: Vec<&str> = mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    for target in targets.iter().rev().filter(|t| **t != "/") {
        let c = sys::cstring(target);
        // SAFETY: The path is a NUL-terminated string.
        unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    }
    if let Err(error) = sys::mount("", "/", "", libc::MS_REMOUNT | libc::MS_RDONLY, None) {
        log!("{error}");
    }
    sys::power_off()
}
