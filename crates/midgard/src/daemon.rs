//! The VM instances' VMs: midgard starts the VM of a VM instance when bifrost asks for a
//! session with it and keeps it running until the guest stops it (see [`Instances`]).
//!
//! Each session becomes a vsock connection to the agent; midgard forwards the bytes
//! without looking at them (bifrost and the agent speak midgard's session protocol).
//!
//! For Wayland forwarding, midgard runs `waypipe client` next to the compositor and
//! connects each connection from the guest's waypipe (vsock port [`WAYPIPE_PORT`]) to it.
//! The compositor is the one of the bifrost that starts the VM, never one from midgard's
//! own environment: systemd starts midgard with the user manager's environment, which
//! need not name the session's display, and a bifrost without a display (e.g. over SSH)
//! must not get windows on someone's screen.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_io::{Async, Timer};
use event_listener::Event;
use futures_lite::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, future};
use midgard_protocol::nonblocking::{read_message, write_message};
use midgard_protocol::session::{Request, Response};
use midgard_protocol::version::Hello;
use midgard_protocol::{AGENT_PORT, WAYPIPE_PORT, control, runtime};
use midgard_vmm::{Machine, MachineHandle, VsockStream};

use crate::instance::{self, Instance};
use crate::{CONSOLE_BUFFER, CONSOLE_CHUNK, boot, log, pull};

/// How long the guest may take to boot (managarm under nested virtualization is slow).
const BOOT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long one ping of the agent may take; then we try again (e.g., if the guest never
/// answers the connection request because its vsock driver is not up).
const PING_TIMEOUT: Duration = Duration::from_secs(10);

/// Waits until the agent answers; returns the version of the guest protocol to speak
/// with it.
async fn wait_for_agent(machine: &Machine) -> Result<u32> {
    let handle = machine.handle();
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if let Some(result) = machine.try_stopped() {
            bail!("{} before the agent started", result?);
        }
        let ping = future::or(async { Some(ping_agent(&handle).await) }, async {
            Timer::after(PING_TIMEOUT).await;
            None
        });
        if let Some(answered) = ping.await
            && let Some(hello) = answered?
        {
            return boot::negotiate(hello.protocol);
        }
        if Instant::now() > deadline {
            bail!(
                "the agent did not start within {} s",
                BOOT_TIMEOUT.as_secs()
            );
        }
        Timer::after(Duration::from_millis(100)).await;
    }
}

/// Returns what the agent answered; None if it does not listen yet.
async fn ping_agent(machine: &MachineHandle) -> Result<Option<Hello>> {
    let mut stream = match machine.vsock_connect(AGENT_PORT).await {
        Ok(stream) => stream,
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => return Ok(None),
        Err(error) => return Err(error).context("failed to connect to the agent"),
    };
    write_message(&mut stream, &Request::Hello(Hello::OURS)).await?;
    match read_message::<Response>(&mut stream).await? {
        Some(Response::Hello(hello)) => Ok(Some(hello)),
        other => bail!("unexpected answer from the agent: {other:?}"),
    }
}

/// Forwards a client connection to a new connection to the agent.
async fn forward(client: Async<UnixStream>, machine: MachineHandle) {
    let guest = match machine.vsock_connect(AGENT_PORT).await {
        Ok(guest) => guest,
        Err(error) => {
            let error = Response::Error(format!("cannot reach the agent: {error}"));
            let _ = write_message(&mut &client, &error).await;
            return;
        }
    };
    proxy(client, guest, "session").await;
}

/// Copies bytes both ways until both sides are done.
async fn proxy(host: Async<UnixStream>, guest: VsockStream, name: &str) {
    let mut guest_writer = guest.try_clone();
    let mut guest_reader = guest;
    let upstream = async {
        let result = copy(&mut &host, &mut guest_writer).await;
        guest_writer.shutdown_write();
        result
    };
    let downstream = async {
        let result = copy(&mut guest_reader, &mut &host).await;
        let _ = host.get_ref().shutdown(Shutdown::Write);
        result
    };
    let (upstream, downstream) = future::zip(upstream, downstream).await;
    let failed = downstream.1.is_some() || upstream.1.is_some();
    if failed || waypipe_debug() {
        log!(
            "{name} connection closed: guest -> host {} bytes{}, host -> guest {} bytes{}",
            downstream.0,
            downstream
                .1
                .map(|e| format!(" (error: {e})"))
                .unwrap_or_default(),
            upstream.0,
            upstream
                .1
                .map(|e| format!(" (error: {e})"))
                .unwrap_or_default(),
        );
    }
}

/// Copies until EOF; returns the byte count and the error that stopped it (if any).
async fn copy(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
) -> (u64, Option<String>) {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0;
    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(0) => return (total, None),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return (total, Some(format!("read: {error}"))),
        };
        if let Err(error) = writer.write_all(&buffer[..count]).await {
            return (total, Some(format!("write: {error}")));
        }
        total += count as u64;
    }
}

/// The `waypipe client` for a VM instance's Wayland clients (see [`forward_waypipe`]).
struct Gui {
    waypipe: Child,
    socket: PathBuf,
}

impl Drop for Gui {
    fn drop(&mut self) {
        let _ = self.waypipe.kill();
        let _ = self.waypipe.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// MIDGARD_WAYPIPE_DEBUG=1: both waypipes log debug messages.
fn waypipe_debug() -> bool {
    std::env::var_os("MIDGARD_WAYPIPE_DEBUG").is_some_and(|value| !value.is_empty() && value != "0")
}

/// Starts `waypipe client` on `socket` for `display` (bifrost's) if there is one and we
/// have waypipe.
fn start_waypipe(socket: PathBuf, display: Option<PathBuf>) -> Option<Gui> {
    let Some(display) = display else {
        log!("bifrost has no Wayland display; Wayland forwarding is off");
        return None;
    };
    let _ = std::fs::remove_file(&socket);
    let program = std::env::var_os("MIDGARD_WAYPIPE").unwrap_or_else(|| "waypipe".into());
    // No compression (the guest's waypipe has none) and no DMABUFs (shared memory only).
    let mut command = Command::new(&program);
    if waypipe_debug() {
        command.arg("--debug");
    }
    command
        .args(["--compress", "none", "--no-gpu", "--socket"])
        .arg(&socket)
        .arg("client")
        .env("WAYLAND_DISPLAY", display)
        .stdin(Stdio::null());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl() is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    match command.spawn() {
        Ok(waypipe) => Some(Gui { waypipe, socket }),
        Err(error) => {
            log!("cannot run {program:?} ({error}); Wayland forwarding is off");
            None
        }
    }
}

/// Connects each connection from the guest's waypipe to `waypipe client`.
fn forward_waypipe(machine: &MachineHandle, gui: &Gui) {
    let listener = match machine.vsock_listen(WAYPIPE_PORT) {
        Ok(listener) => listener,
        Err(error) => {
            log!("cannot listen on vsock port {WAYPIPE_PORT}: {error}");
            return;
        }
    };
    let socket = gui.socket.clone();
    let spawner = machine.clone();
    machine.spawn(async move {
        loop {
            let guest = listener.accept().await;
            let socket = socket.clone();
            spawner.spawn(async move {
                match connect_waypipe(&socket).await {
                    Ok(host) => proxy(host, guest, "waypipe").await,
                    Err(error) => log!("cannot connect to waypipe: {error}"),
                }
            });
        }
    });
}

/// Connects to `waypipe client`'s socket (which it creates soon after it started).
async fn connect_waypipe(socket: &Path) -> io::Result<Async<UnixStream>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Async::<UnixStream>::connect(socket).await {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() > deadline => return Err(error),
            Err(_) => {
                Timer::after(Duration::from_millis(20)).await;
            }
        }
    }
}

/// The state of a VM instance's VM.
enum State {
    /// The VM boots; the agent does not answer yet.
    Booting,
    /// The agent answers (and speaks this version of the guest protocol); sessions run as
    /// tasks on the VM's executor.
    Ready(MachineHandle, u32),
    /// The VM did not boot, or it stopped.
    Gone(String),
}

/// A VM instance's VM, which runs on its own thread (see [`run_instance`]).
struct Running {
    state: Mutex<State>,
    changed: Event,
    /// The VM, once it exists (also while it boots).
    machine: OnceLock<MachineHandle>,
}

impl Running {
    fn set(&self, state: State) {
        *self.state.lock().unwrap() = state;
        self.changed.notify(usize::MAX);
    }

    fn booting(&self) -> bool {
        matches!(*self.state.lock().unwrap(), State::Booting)
    }

    /// Waits until the VM no longer boots.
    async fn booted(&self) {
        loop {
            let listener = self.changed.listen();
            if !self.booting() {
                return;
            }
            listener.await;
        }
    }
}

/// The VMs of VM instances that run (or start), by name, and the instances that are
/// being created or deleted (whose VMs cannot start meanwhile). Whoever locks both takes
/// `running` first.
#[derive(Default)]
pub struct Instances {
    running: Arc<Mutex<BTreeMap<String, Arc<Running>>>>,
    reserved: Mutex<BTreeSet<String>>,
}

/// A VM instance that is being created or deleted (see [`Instances::reserve`]).
pub struct Reservation<'a> {
    instances: &'a Instances,
    name: String,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.instances.reserved.lock().unwrap().remove(&self.name);
    }
}

async fn respond(stream: &Async<UnixStream>, response: control::Response) -> Result<()> {
    write_message(&mut &*stream, &response).await?;
    Ok(())
}

impl Instances {
    pub fn is_running(&self, name: &str) -> bool {
        self.running.lock().unwrap().contains_key(name)
    }

    pub fn running(&self) -> Vec<String> {
        self.running.lock().unwrap().keys().cloned().collect()
    }

    /// Reserves the VM instance `name` for creating or deleting it, which fails if its VM
    /// runs or someone else reserved it.
    pub fn reserve(&self, name: &str) -> Result<Reservation<'_>> {
        let running = self.running.lock().unwrap();
        let mut reserved = self.reserved.lock().unwrap();
        if running.contains_key(name) {
            bail!("{name} is running (bifrost shutdown {name} stops it)");
        }
        if !reserved.insert(name.to_string()) {
            bail!("{name} is being created or deleted");
        }
        Ok(Reservation {
            instances: self,
            name: name.to_string(),
        })
    }

    /// The counters of the VM of `name` (see [`control::Request::Counters`]; they may take
    /// a moment to collect).
    pub fn counters(&self, name: &str) -> control::Response {
        if let Err(error) = instance::validate_name(name) {
            return control::Response::Error(format!("{error:#}"));
        }
        let running = self.running.lock().unwrap().get(name).cloned();
        let machine = running.as_ref().and_then(|running| running.machine.get());
        match machine {
            Some(machine) => match machine.counters() {
                Ok(counters) => control::Response::Counters(counters),
                Err(error) => control::Response::Error(format!("{error:#}")),
            },
            None if running.is_none() && !instance::exists(name) => control::Response::NoInstance,
            None => control::Response::Error(format!("{name} is not running")),
        }
    }

    /// Connects `stream` to the agent in the VM of `name` (see
    /// [`control::Request::Session`]).
    pub async fn session(
        &self,
        stream: Async<UnixStream>,
        name: &str,
        start: bool,
        display: Option<PathBuf>,
    ) -> Result<()> {
        if let Err(error) = instance::validate_name(name) {
            return respond(&stream, control::Response::Error(format!("{error:#}"))).await;
        }
        let running = {
            let mut instances = self.running.lock().unwrap();
            match instances.get(name) {
                Some(running) => Ok(running.clone()),
                None if self.reserved.lock().unwrap().contains(name) => Err(
                    control::Response::Error(format!("{name} is being created or deleted")),
                ),
                None if !instance::exists(name) => Err(control::Response::NoInstance),
                None if !start => Err(control::Response::Error(format!("{name} is not running"))),
                None => {
                    let running = Arc::new(Running {
                        state: Mutex::new(State::Booting),
                        changed: Event::new(),
                        machine: OnceLock::new(),
                    });
                    instances.insert(name.to_string(), running.clone());
                    let instances = self.running.clone();
                    let name = name.to_string();
                    let thread_running = running.clone();
                    std::thread::spawn(move || {
                        run_instance(&name, display, &thread_running);
                        let mut instances = instances.lock().unwrap();
                        if instances
                            .get(&name)
                            .is_some_and(|entry| Arc::ptr_eq(entry, &thread_running))
                        {
                            instances.remove(&name);
                        }
                    });
                    Ok(running)
                }
            }
        };
        let running = match running {
            Ok(running) => running,
            Err(response) => return respond(&stream, response).await,
        };
        if running.booting() {
            respond(&stream, control::Response::Starting).await?;
            running.booted().await;
        }
        let ready = match &*running.state.lock().unwrap() {
            State::Booting => unreachable!(),
            State::Ready(machine, protocol) => Ok((machine.clone(), *protocol)),
            State::Gone(reason) => Err(reason.clone()),
        };
        match ready {
            Ok((machine, protocol)) => {
                respond(&stream, control::Response::Ready { protocol }).await?;
                // The session's task runs on the VM's executor.
                machine.spawn(forward(stream, machine.clone()));
            }
            Err(reason) => respond(&stream, control::Response::Error(reason)).await?,
        }
        Ok(())
    }
}

/// Writes the console output of a VM into its console log, on a thread of its own.
/// Dropping it waits until the thread wrote everything, which it does once the VM is
/// gone (and the pipe closed).
struct ConsoleLog(Option<JoinHandle<()>>);

impl ConsoleLog {
    /// Writes what the returned pipe gets into `log` (of the VM of `name`).
    fn start(name: &str, mut log: File) -> Result<(Self, piper::Writer)> {
        let (mut output, console) = piper::pipe(CONSOLE_BUFFER);
        let thread = std::thread::Builder::new()
            .name(format!("{name}-console"))
            .spawn(move || {
                let mut buffer = vec![0u8; CONSOLE_CHUNK];
                // What has piled up is written at once.
                while let Ok(count @ 1..) = future::block_on(output.read(&mut buffer)) {
                    let _ = log.write_all(&buffer[..count]);
                }
            })
            .context("failed to start the console's thread")?;
        Ok((Self(Some(thread)), console))
    }
}

impl Drop for ConsoleLog {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            let _ = thread.join();
        }
    }
}

/// Runs the VM of `name` (see [`Instance::config`]) until it stops; its Wayland clients go
/// to `display` (bifrost's), if any.
fn run_instance(name: &str, display: Option<PathBuf>, running: &Running) {
    let reason = match boot_instance(name, display, running) {
        Ok(reason) => reason,
        Err(error) => format!("{error:#} (its console: bifrost log {name})"),
    };
    log!("{name}: {reason}");
    running.set(State::Gone(format!("{name}: {reason}")));
}

/// Boots the VM of `name`, makes it ready for sessions and runs it until it stops;
/// returns why it stopped.
fn boot_instance(name: &str, display: Option<PathBuf>, running: &Running) -> Result<String> {
    // A pull does not replace the boot files until the VM read them.
    let files = pull::reading();
    let mut config = Instance::load(name)?.config()?;
    config.guest.waypipe_debug = waypipe_debug();
    let dir = runtime::runtime_dir(name);
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {dir:?}"))?;
    let path = runtime::console_log(name);
    let log = File::create(&path).with_context(|| format!("failed to create {path:?}"))?;
    // Dropped after the VM, such that the log is complete once we report why it stopped.
    let (console_log, console) = ConsoleLog::start(name, log)?;

    // The guest gets a Wayland display only if we can forward its clients.
    let gui = config
        .waypipe
        .is_some()
        .then(|| start_waypipe(runtime::waypipe_socket(name), display))
        .flatten();
    if gui.is_none() {
        config.waypipe = None;
    }
    let machine = midgard_vmm::start(&config, console, None)?;
    drop(files);
    let handle = machine.handle();
    let _ = running.machine.set(handle.clone());
    if let Some(gui) = &gui {
        forward_waypipe(&handle, gui);
    }
    let protocol = match machine.run(wait_for_agent(&machine)) {
        Ok(protocol) => protocol,
        Err(error) => {
            handle.stop();
            let _ = machine.run_until_stopped();
            return Err(error);
        }
    };
    log!("{name} is ready");
    running.set(State::Ready(handle, protocol));

    let result = machine.run_until_stopped();
    drop(gui);
    drop(machine);
    drop(console_log);
    Ok(result?.to_string())
}
