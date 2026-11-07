//! Talking to midgard (on its socket, see [`midgard_protocol::control`]): runs commands
//! (or a login shell) in a VM instance. midgard starts an instance's VM for the first
//! session with it; the VM stays up for later commands.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use midgard_protocol::session::{Exec, Request, Response, WindowSize};
use midgard_protocol::{control, runtime};

use crate::cli::RunArgs;
use crate::{log, terminal};

// The libc crate does not define these for managarm; mlibc uses the Linux ABI values.
const TIOCGWINSZ: libc::c_ulong = 0x5413;
const SA_RESTART: libc::c_int = 0x1000_0000;

/// Signals that we forward to the command.
const FORWARDED_SIGNALS: [i32; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty() only takes a file descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

fn window_size() -> Option<WindowSize> {
    // SAFETY: TIOCGWINSZ writes a struct winsize.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        (libc::ioctl(1, TIOCGWINSZ as _, &mut size) == 0).then_some(WindowSize {
            rows: size.ws_row,
            cols: size.ws_col,
        })
    }
}

/// Connects to midgard's socket (systemd starts midgard on the first connection).
pub fn connect_daemon() -> Result<UnixStream> {
    let socket = runtime::daemon_socket();
    UnixStream::connect(&socket).with_context(|| {
        format!(
            "failed to connect to midgard at {} (is midgard.socket running?)",
            socket.display()
        )
    })
}

/// Connects to the agent in the VM of `name`, which midgard starts first if it does
/// not run (and `start`).
fn connect(name: &str, start: bool) -> Result<UnixStream> {
    let mut stream = connect_daemon()?;
    let request = control::Request::Session {
        name: name.to_string(),
        start,
    };
    midgard_protocol::write_message(&mut stream, &request)?;
    loop {
        match midgard_protocol::read_message(&mut stream)
            .context("lost the connection to midgard")?
        {
            Some(control::Response::Starting) => {
                if is_tty(2) {
                    log!("starting {name}...");
                }
            }
            Some(control::Response::Ready) => return Ok(stream),
            Some(control::Response::Error(message)) => bail!("{message}"),
            Some(other) => bail!("unexpected answer from midgard: {other:?}"),
            None => bail!("midgard closed the connection"),
        }
    }
}

// Signals reach us through a pipe (the handler only writes the signal number).
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: i32) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    let byte = signal as u8;
    // SAFETY: write() is async-signal-safe.
    unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
}

fn install_signal_handlers(tty: bool) -> Result<std::fs::File> {
    let mut fds = [0; 2];
    // SAFETY: pipe2() writes two file descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error()).context("pipe2() failed");
    }
    SIGNAL_PIPE.store(fds[1], Ordering::Relaxed);
    let mut signals = FORWARDED_SIGNALS.to_vec();
    if tty {
        signals.push(libc::SIGWINCH);
    }
    for signal in signals {
        // SAFETY: The handler only calls write().
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            action.sa_flags = SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    // SAFETY: We own the read end.
    Ok(unsafe { std::fs::File::from_raw_fd(fds[0]) })
}

/// Runs a command in the VM of `name`; returns its exit code.
pub fn run(name: &str, args: &RunArgs) -> Result<i32> {
    let stream = connect(name, !args.no_start)?;
    let tty = args.tty().unwrap_or_else(|| is_tty(0) && is_tty(1));
    let mut env = Vec::new();
    if tty && let Ok(term) = std::env::var("TERM") {
        env.push(("TERM".to_string(), term));
    }
    env.extend(
        args.env()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    let exec = Exec {
        argv: args.command.clone(),
        env,
        // The agent falls back to the home directory.
        cwd: args.cwd.clone(),
        tty: if tty {
            Some(window_size().unwrap_or(WindowSize { rows: 24, cols: 80 }))
        } else {
            None
        },
    };
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    midgard_protocol::write_message(&mut *writer.lock().unwrap(), &Request::Exec(exec))?;
    if tty {
        terminal::enter_raw_mode(false);
    }
    let result = session(stream, writer, tty);
    terminal::restore();
    result
}

fn session(stream: UnixStream, writer: Arc<Mutex<UnixStream>>, tty: bool) -> Result<i32> {
    let send = |writer: &Arc<Mutex<UnixStream>>, request: Request| {
        let _ = midgard_protocol::write_message(&mut *writer.lock().unwrap(), &request);
    };

    // stdin -> the command. When the session ends, `quit` stops the thread before we
    // exit: managarm does not cancel a read or poll on the terminal that is still pending
    // when a process exits (the read takes the next input from the shell that started us,
    // and exiting with a pending poll can hang).
    let (quit_read, mut quit_write) = {
        let mut fds = [0; 2];
        // SAFETY: pipe2() writes two file descriptors.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error()).context("pipe2() failed");
        }
        // SAFETY: We own the new file descriptors.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    };
    let (stopped_sender, stopped) = std::sync::mpsc::channel::<()>();
    let input_writer = writer.clone();
    std::thread::spawn(move || {
        let _stopped = stopped_sender;
        let mut stdin = io::stdin().lock();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            // Read only once input is there (see above).
            let mut fds = [
                libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: quit_read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: poll() gets two valid pollfds.
            if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                send(&input_writer, Request::StdinEof);
                return;
            }
            if fds[1].revents != 0 {
                return;
            }
            match stdin.read(&mut buffer) {
                // Our signal handlers (e.g., SIGWINCH on a resize) interrupt the read:
                // managarm does not restart it despite SA_RESTART.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(0) | Err(_) => {
                    send(&input_writer, Request::StdinEof);
                    return;
                }
                Ok(count) => send(&input_writer, Request::Stdin(buffer[..count].to_vec())),
            }
        }
    });
    let mut stop_input = move || {
        let _ = quit_write.write_all(&[0]);
        let _ = stopped.recv_timeout(std::time::Duration::from_secs(1));
    };

    // Signals and window size changes -> the command.
    let mut signals = install_signal_handlers(tty)?;
    let signal_writer = writer.clone();
    std::thread::spawn(move || {
        let mut byte = [0u8];
        while signals.read_exact(&mut byte).is_ok() {
            let signal = byte[0] as i32;
            if signal == libc::SIGWINCH {
                if let Some(size) = window_size() {
                    send(&signal_writer, Request::Resize(size));
                }
            } else {
                send(&signal_writer, Request::Signal(signal));
            }
        }
    });

    // The command's output -> stdout / stderr.
    let result = (|| -> Result<i32> {
        let mut reader = stream;
        let mut stdout = io::stdout();
        let mut stderr = io::stderr();
        loop {
            match midgard_protocol::read_message::<Response>(&mut reader)
                .context("lost the connection to the VM")?
            {
                Some(Response::Started { .. }) => {}
                Some(Response::Stdout(data)) => {
                    stdout.write_all(&data)?;
                    stdout.flush()?;
                }
                Some(Response::Stderr(data)) => {
                    stderr.write_all(&data)?;
                    stderr.flush()?;
                }
                Some(Response::Exited { code, signal }) => {
                    return Ok(code.unwrap_or_else(|| 128 + signal.unwrap_or(0)));
                }
                Some(Response::Error(message)) => bail!("{message}"),
                Some(Response::Pong) => {}
                None => bail!("the VM closed the connection"),
            }
        }
    })();
    stop_input();
    result
}
