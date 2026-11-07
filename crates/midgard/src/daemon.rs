//! The VM instances' VMs: midgard starts the VM of a VM instance when bifrost asks for a
//! session with it and keeps it running until the guest stops it (see [`Instances`]).
//!
//! Each session becomes a vsock connection to the agent; midgard forwards the bytes
//! without looking at them (bifrost and the agent speak midgard's session protocol).

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_io::{Async, Timer};
use event_listener::Event;
use futures_lite::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, future};
use midgard_protocol::nonblocking::{read_message, write_message};
use midgard_protocol::session::{Request, Response};
use midgard_protocol::{AGENT_PORT, control, runtime};
use midgard_vmm::{Machine, MachineHandle, VsockStream};

use crate::instance;
use crate::log;

/// How long the guest may take to boot (managarm under nested virtualization is slow).
const BOOT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long one ping of the agent may take; then we try again (e.g., if the guest never
/// answers the connection request because its vsock driver is not up).
const PING_TIMEOUT: Duration = Duration::from_secs(10);

/// Waits until the agent answers.
async fn wait_for_agent(machine: &Machine) -> Result<()> {
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
            && answered?
        {
            return Ok(());
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

/// Returns whether the agent answered; false if it does not listen yet.
async fn ping_agent(machine: &MachineHandle) -> Result<bool> {
    let mut stream = match machine.vsock_connect(AGENT_PORT).await {
        Ok(stream) => stream,
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => return Ok(false),
        Err(error) => return Err(error).context("failed to connect to the agent"),
    };
    write_message(&mut stream, &Request::Ping).await?;
    match read_message::<Response>(&mut stream).await? {
        Some(Response::Pong) => Ok(true),
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
    proxy(client, guest).await;
}

/// Copies bytes both ways until both sides are done.
async fn proxy(host: Async<UnixStream>, guest: VsockStream) {
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
    if downstream.1.is_some() || upstream.1.is_some() {
        log!(
            "session connection closed: guest -> host {} bytes{}, host -> guest {} bytes{}",
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

/// The state of a VM instance's VM.
enum State {
    /// The VM boots; the agent does not answer yet.
    Booting,
    /// The agent answers; sessions run as tasks on the VM's executor.
    Ready(MachineHandle),
    /// The VM did not boot, or it stopped.
    Gone(String),
}

/// A VM instance's VM, which runs on its own thread (see [`run_instance`]).
struct Running {
    state: Mutex<State>,
    changed: Event,
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

/// The VMs of VM instances that run (or start), by name.
#[derive(Default)]
pub struct Instances {
    running: Arc<Mutex<BTreeMap<String, Arc<Running>>>>,
}

async fn respond(stream: &Async<UnixStream>, response: control::Response) -> Result<()> {
    write_message(&mut &*stream, &response).await?;
    Ok(())
}

impl Instances {
    pub fn running(&self) -> Vec<String> {
        self.running.lock().unwrap().keys().cloned().collect()
    }

    /// Connects `stream` to the agent in the VM of `name` (see
    /// [`control::Request::Session`]).
    pub async fn session(&self, stream: Async<UnixStream>, name: &str, start: bool) -> Result<()> {
        if let Err(error) = instance::validate_name(name) {
            return respond(&stream, control::Response::Error(format!("{error:#}"))).await;
        }
        let running = {
            let mut instances = self.running.lock().unwrap();
            match instances.get(name) {
                Some(running) => Some(running.clone()),
                None if !start => None,
                None => {
                    let running = Arc::new(Running {
                        state: Mutex::new(State::Booting),
                        changed: Event::new(),
                    });
                    instances.insert(name.to_string(), running.clone());
                    let instances = self.running.clone();
                    let name = name.to_string();
                    let thread_running = running.clone();
                    std::thread::spawn(move || {
                        run_instance(&name, &thread_running);
                        let mut instances = instances.lock().unwrap();
                        if instances
                            .get(&name)
                            .is_some_and(|entry| Arc::ptr_eq(entry, &thread_running))
                        {
                            instances.remove(&name);
                        }
                    });
                    Some(running)
                }
            }
        };
        let Some(running) = running else {
            let error = format!("{name} is not running");
            return respond(&stream, control::Response::Error(error)).await;
        };
        if running.booting() {
            respond(&stream, control::Response::Starting).await?;
            running.booted().await;
        }
        let ready = match &*running.state.lock().unwrap() {
            State::Booting => unreachable!(),
            State::Ready(machine) => Ok(machine.clone()),
            State::Gone(reason) => Err(reason.clone()),
        };
        match ready {
            Ok(machine) => {
                respond(&stream, control::Response::Ready).await?;
                // The session's task runs on the VM's executor.
                machine.spawn(forward(stream, machine.clone()));
            }
            Err(reason) => respond(&stream, control::Response::Error(reason)).await?,
        }
        Ok(())
    }
}

/// Runs the VM of `name` (see [`instance::config`]) until it stops.
fn run_instance(name: &str, running: &Running) {
    let reason = match boot_instance(name, running) {
        Ok(reason) => reason,
        Err(error) => format!(
            "{error:#} (its console: {})",
            runtime::console_log(name).display()
        ),
    };
    log!("{name}: {reason}");
    running.set(State::Gone(format!("{name}: {reason}")));
}

/// Boots the VM of `name`, makes it ready for sessions and runs it until it stops;
/// returns why it stopped.
fn boot_instance(name: &str, running: &Running) -> Result<String> {
    let config = instance::config(name)?;
    let dir = runtime::runtime_dir(name);
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {dir:?}"))?;
    let path = runtime::console_log(name);
    let console = File::create(&path).with_context(|| format!("failed to create {path:?}"))?;

    let machine = midgard_vmm::start(&config, Box::new(console))?;
    let handle = machine.handle();
    if let Err(error) = machine.run(wait_for_agent(&machine)) {
        handle.stop();
        let _ = machine.run_until_stopped();
        return Err(error);
    }
    log!("{name} is ready");
    running.set(State::Ready(handle));

    Ok(machine.run_until_stopped()?.to_string())
}
