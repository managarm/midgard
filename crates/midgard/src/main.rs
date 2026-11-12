//! midgard: runs Linux in lightweight VMs on managarm (see [`midgard_vmm`]). It is a
//! daemon that systemd starts through socket activation (see the unit files in
//! `systemd/`) once bifrost connects to its socket. bifrost asks it to run VMs (see
//! [`midgard_protocol::control`]): VMs in the foreground that bifrost describes, and the
//! VMs of VM instances, which midgard keeps running for bifrost's sessions (see
//! [`daemon`]). midgard also manages the VM instances (see [`instance`]), the templates
//! that they are created from (see [`template`]) and the boot files that they boot with
//! (see [`boot`]), which it installs (see [`pull`]).
//!
//! The connections to bifrost are tasks on one executor ([`EXECUTOR`], on the main
//! thread). Each VM runs on a thread of its own with its own executor (see
//! [`midgard_vmm::Machine::run`]; its vCPUs have threads of their own); a VM in the
//! foreground reports to its connection's task through channels (see [`Event`] and
//! [`boot`]). Work on files that takes a while (e.g., creating a VM instance) runs on
//! [`blocking`]'s thread pool.

mod boot;
mod daemon;
mod instance;
mod paths;
mod pull;
mod settings;
mod template;

use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, anyhow, bail};
use async_channel::TrySendError;
use async_executor::Executor;
use async_io::Async;
use blocking::unblock;
use clap::Parser;
use futures_lite::{AsyncReadExt, future};
use midgard_protocol::control::{Listing, Pull, Request, Response};
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
/// The console output that we buffer for its reader (bifrost, or the console log of a VM
/// instance); more is dropped.
pub(crate) const CONSOLE_BUFFER: usize = 1 << 20;
/// The most console output that we pass on at once.
pub(crate) const CONSOLE_CHUNK: usize = 64 << 10;
/// Console input (in messages) that waits for the guest; more is dropped (see
/// [`forward_input`]).
const INPUT_QUEUE: usize = 1024;

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
    /// The VM stopped (why), or did not start. Its console is closed by then.
    Stopped(Result<String>),
}

/// Runs a VM on this (its own) thread until it stops.
fn run_vm(
    config: &MachineConfig,
    console: piper::Writer,
    input: async_channel::Receiver<Vec<u8>>,
    events: async_channel::Sender<Event>,
) {
    let result = midgard_vmm::start(config, console, Some(input)).and_then(|machine| {
        let _ = events.send_blocking(Event::Started(machine.handle()));
        Ok(machine.run_until_stopped()?.to_string())
    });
    let _ = events.send_blocking(Event::Stopped(result));
}

/// Passes bifrost's input to the VM's console; stops the VM when bifrost closes the
/// connection. To notice that, we keep reading: input that the guest does not take is
/// dropped once [`INPUT_QUEUE`] messages wait.
async fn forward_input(
    stream: Arc<Async<UnixStream>>,
    input: async_channel::Sender<Vec<u8>>,
    machine: MachineHandle,
) {
    let mut dropped = false;
    while let Ok(Some(Request::Input(data))) = read_message(&mut &*stream).await {
        if let Err(TrySendError::Full(_)) = input.try_send(data)
            && !dropped
        {
            log!("dropping console input that the guest does not read");
            dropped = true;
        }
    }
    machine.stop();
}

/// Sends the VM's console output to bifrost until the VM is gone.
async fn forward_console(stream: &Async<UnixStream>, mut output: piper::Reader) {
    let mut buffer = vec![0u8; CONSOLE_CHUNK];
    // What has piled up goes in one message.
    while let Ok(count @ 1..) = output.read(&mut buffer).await {
        let data = buffer[..count].to_vec();
        // If bifrost is gone, the input's task stops the VM.
        let _ = write_message(&mut &*stream, &Response::Console(data)).await;
    }
}

/// Runs a VM until it stops (or bifrost goes away); returns why it stopped.
async fn boot(stream: &Arc<Async<UnixStream>>, config: MachineConfig) -> Result<String> {
    let (output, console) = piper::pipe(CONSOLE_BUFFER);
    let (input, input_receiver) = async_channel::bounded(INPUT_QUEUE);
    let (sender, events) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("vm".into())
        .spawn(move || run_vm(&config, console, input_receiver, sender))
        .context("failed to start the VM's thread")?;
    let watch_vm = async {
        let mut forwarder = None;
        while let Ok(event) = events.recv().await {
            match event {
                Event::Started(machine) => {
                    let task = forward_input(stream.clone(), input.clone(), machine);
                    forwarder = Some(EXECUTOR.spawn(task));
                }
                Event::Stopped(result) => {
                    drop(forwarder);
                    return result;
                }
            }
        }
        bail!("the VM's thread exited")
    };
    // All of the console output reaches bifrost before why the VM stopped.
    let ((), result) = future::zip(forward_console(stream, output), watch_vm).await;
    result
}

/// Runs `job` on [`blocking`]'s thread pool and answers [`Response::Done`] or its error.
async fn answer(job: impl FnOnce() -> Result<()> + Send + 'static) -> Response {
    match unblock(job).await {
        Ok(()) => Response::Done,
        Err(error) => Response::Error(format!("{error:#}")),
    }
}

/// Pulls on [`blocking`]'s thread pool and tells bifrost what happens; once bifrost is
/// gone, the pull stops.
async fn pull(stream: &Async<UnixStream>, request: Pull) -> Response {
    let (sender, events) = async_channel::bounded(16);
    let job = unblock(move || {
        pull::pull(&request, &mut |event| {
            sender
                .send_blocking(event)
                .map_err(|_| anyhow!("bifrost closed the connection"))
        })
    });
    let forward = async {
        while let Ok(event) = events.recv().await {
            if write_message(&mut &*stream, &Response::Pull(event))
                .await
                .is_err()
            {
                events.close();
            }
        }
    };
    match future::zip(forward, job).await {
        ((), Ok(())) => Response::Done,
        ((), Err(error)) => Response::Error(format!("{error:#}")),
    }
}

/// The VM instances and the templates.
fn list() -> Listing {
    let instances = paths::names(&paths::instances_dir())
        .iter()
        .map(|name| instance::info(name, INSTANCES.is_running(name)))
        .collect();
    let templates = paths::names(&paths::templates_dir())
        .iter()
        .map(|name| template::info(name))
        .collect();
    Listing {
        instances,
        templates,
        templates_dir: paths::templates_dir(),
    }
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
            match boot(&stream, *config).await {
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
        Request::Input(_) => Response::Error("no VM runs on this connection".into()),
        Request::List => Response::List(list()),
        Request::Create {
            name,
            template,
            user,
            memory,
        } => {
            answer(move || {
                let _reservation = INSTANCES.reserve(&name)?;
                instance::create(&name, &template, user.as_deref(), memory)
            })
            .await
        }
        Request::Delete { name } => {
            answer(move || {
                let _reservation = INSTANCES.reserve(&name)?;
                instance::delete(&name)
            })
            .await
        }
        Request::Pull(request) => pull(&stream, request).await,
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
    // (Before any request can create or delete instances.)
    instance::remove_leftovers();
    pull::remove_leftovers();
    async_io::block_on(EXECUTOR.run(accept(listener)));
}
