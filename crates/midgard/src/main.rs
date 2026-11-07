//! midgard: runs Linux in lightweight VMs on managarm (see [`midgard_vmm`]). It is a
//! daemon that systemd starts through socket activation (see the unit files in
//! `systemd/`) once bifrost connects to its socket. bifrost asks it to run VMs (see
//! [`midgard_protocol::control`]): VMs in the foreground that bifrost describes, and the
//! VMs of VM instances, which midgard keeps running for bifrost's sessions (see
//! [`daemon`]).
//!
//! The connections to bifrost are tasks on one executor ([`EXECUTOR`], on the main
//! thread). Each VM runs on a thread of its own with its own executor (see
//! [`midgard_vmm::Machine::run`]; its vCPUs have threads of their own); a VM in the
//! foreground reports to its connection's task through a channel (see [`Event`]).

mod daemon;
mod instance;
mod paths;

use std::io::{self, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, bail};
use async_executor::Executor;
use async_io::Async;
use clap::Parser;
use midgard_protocol::control::{Request, Response};
use midgard_protocol::machine::MachineConfig;
use midgard_protocol::nonblocking::{read_message, write_message};
use midgard_protocol::runtime;
use midgard_vmm::MachineHandle;

use crate::daemon::Instances;

/// Prints a message (like `eprintln!`) with midgard's prefix; it goes to the console log.
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("midgard: {}", format_args!($($arg)*)) };
}
pub(crate) use log;

/// The daemon that runs Linux VMs for bifrost (systemd starts it when bifrost connects
/// to its socket).
#[derive(Parser)]
#[command(version)]
struct Cli {}

/// Runs the connections to bifrost.
static EXECUTOR: Executor<'static> = Executor::new();

/// The VM instances' VMs.
static INSTANCES: LazyLock<Instances> = LazyLock::new(Instances::default);

/// The first file descriptor that systemd passes (see sd_listen_fds(3)).
const LISTEN_FDS_START: RawFd = 3;
/// Console output that we buffer (in messages) before the guest has to wait for bifrost.
const CONSOLE_QUEUE: usize = 256;

/// The socket that systemd passes us (socket activation), if it does.
fn systemd_listener() -> Result<Option<UnixListener>> {
    let pid = std::env::var("LISTEN_PID").ok();
    let fds = std::env::var("LISTEN_FDS").ok();
    if pid.and_then(|pid| pid.parse::<u32>().ok()) != Some(std::process::id()) {
        return Ok(None);
    }
    // SAFETY: We have not started any threads yet.
    unsafe {
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    if fds.as_deref() != Some("1") {
        bail!("systemd must pass exactly one socket (LISTEN_FDS={fds:?})");
    }
    // Do not leak the socket into processes that we start.
    // SAFETY: fcntl() only takes a file descriptor.
    if unsafe { libc::fcntl(LISTEN_FDS_START, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error()).context("the socket from systemd is invalid");
    }
    // SAFETY: systemd passes us the listening socket; nothing else uses it.
    Ok(Some(unsafe { UnixListener::from_raw_fd(LISTEN_FDS_START) }))
}

/// Listens on our socket ourselves (if midgard runs without systemd).
fn own_listener() -> Result<UnixListener> {
    let socket = runtime::daemon_socket();
    if UnixStream::connect(&socket).is_ok() {
        bail!("midgard is running already ({})", socket.display());
    }
    if let Some(dir) = socket.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let _ = std::fs::remove_file(&socket);
    UnixListener::bind(&socket).with_context(|| format!("failed to listen on {}", socket.display()))
}

/// What a VM's thread tells the task of its connection.
enum Event {
    /// The VM exists (its console may have written before).
    Started(MachineHandle),
    /// Output of the VM's console.
    Console(Vec<u8>),
    /// The VM stopped (why), or did not start.
    Stopped(Result<String>),
}

/// The VM's console: [`Event::Console`]s for the connection's task. The guest waits
/// while the queue is full (like for a slow serial line).
struct Console(async_channel::Sender<Event>);

impl Write for Console {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0
            .send_blocking(Event::Console(data.to_vec()))
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Runs a VM on this (its own) thread until it stops.
fn run_vm(config: &MachineConfig, events: async_channel::Sender<Event>) {
    let console = Console(events.clone());
    let result = midgard_vmm::start(config, Box::new(console)).and_then(|machine| {
        let _ = events.send_blocking(Event::Started(machine.handle()));
        Ok(machine.run_until_stopped()?.to_string())
    });
    let _ = events.send_blocking(Event::Stopped(result));
}

/// Stops the VM when bifrost closes the connection.
async fn watch(stream: Arc<Async<UnixStream>>, machine: MachineHandle) {
    let _ = read_message::<Request>(&mut &*stream).await;
    machine.stop();
}

/// Runs a VM until it stops (or bifrost goes away); returns why it stopped.
async fn boot(stream: &Arc<Async<UnixStream>>, config: MachineConfig) -> Result<String> {
    let (sender, events) = async_channel::bounded(CONSOLE_QUEUE);
    std::thread::Builder::new()
        .name("vm".into())
        .spawn(move || run_vm(&config, sender))
        .context("failed to start the VM's thread")?;
    let mut watcher = None;
    while let Ok(event) = events.recv().await {
        match event {
            Event::Started(machine) => {
                watcher = Some(EXECUTOR.spawn(watch(stream.clone(), machine)))
            }
            Event::Console(mut data) => {
                // Send what has piled up in one message.
                while let Ok(Event::Console(more)) = events.try_recv() {
                    data.extend_from_slice(&more);
                }
                // If bifrost is gone, the watcher stops the VM.
                let _ = write_message(&mut &**stream, &Response::Console(data)).await;
            }
            Event::Stopped(result) => {
                drop(watcher);
                return result;
            }
        }
    }
    bail!("the VM's thread exited")
}

/// Serves one connection from bifrost.
async fn serve(stream: Async<UnixStream>) -> Result<()> {
    let Some(request) = read_message::<Request>(&mut &stream).await? else {
        return Ok(());
    };
    let stream = match request {
        // The session takes the connection over (if it gets to the agent).
        Request::Session { name, start } => {
            return INSTANCES.session(stream, &name, start).await;
        }
        _ => Arc::new(stream),
    };
    let response = match request {
        Request::Boot(config) => {
            let kernel = config.kernel.display().to_string();
            match boot(&stream, config).await {
                Ok(reason) => {
                    log!("{kernel}: {reason}");
                    Response::Stopped(reason)
                }
                Err(error) => {
                    log!("{kernel}: {error:#}");
                    Response::Error(format!("{error:#}"))
                }
            }
        }
        Request::Running => Response::Running(INSTANCES.running()),
        Request::Session { .. } => unreachable!(),
    };
    // bifrost may be gone already (which stopped the VM).
    let _ = write_message(&mut &*stream, &response).await;
    Ok(())
}

/// Accepts connections from bifrost; each one becomes a task.
async fn accept(listener: Async<UnixListener>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => EXECUTOR
                .spawn(async move {
                    if let Err(error) = serve(stream).await {
                        log!("{error:#}");
                    }
                })
                .detach(),
            Err(error) => log!("failed to accept a connection: {error}"),
        }
    }
}

fn main() {
    Cli::parse();
    let listener = systemd_listener()
        .transpose()
        .unwrap_or_else(own_listener)
        .and_then(|listener| Ok(Async::new(listener)?));
    let listener = match listener {
        Ok(listener) => listener,
        Err(error) => {
            log!("{error:#}");
            std::process::exit(1);
        }
    };
    async_io::block_on(EXECUTOR.run(accept(listener)));
}
