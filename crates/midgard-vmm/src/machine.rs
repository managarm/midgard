//! Setting up, starting and stopping a VM.

use std::cell::OnceCell;
use std::fmt;
use std::future::Future;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use async_executor::Executor;
use event_listener::Event;
use midgard_protocol::control::Counters;
use midgard_protocol::guest::Ready;
use midgard_protocol::machine::{ConsoleKind, MachineConfig};
use midgard_protocol::{SHARE_TAG, param};

use crate::board::Board;
use crate::host::console::ConsoleOutput;
use crate::host::{image, net};
use crate::logging::log;
use crate::virtio::block::{self, Block};
use crate::virtio::console::{self, Console};
use crate::virtio::fs::{self, VirtioFs};
use crate::virtio::net::VirtioNet;
use crate::virtio::vsock::{Vsock, VsockListener, VsockStream};
use crate::virtio::{DeviceList, DeviceRef};
use crate::{acpi, boot, platform, vcpu, virtio};

/// Kernel parameters that every guest needs (unless the configuration replaces them);
/// the console and the virtio devices are added.
const BASE_CMDLINE: &str = "reboot=k panic=-1";
/// The default number of vCPUs: the host's CPUs, up to this many.
const DEFAULT_MAX_CPUS: u32 = 8;

/// The most vCPUs that a VM can have (on this hypervisor).
// Only some backends (Hel) have a limit below the platform's.
#[allow(clippy::unnecessary_min_or_max)]
fn max_cpus() -> u32 {
    platform::MAX_CPUS.min(midgard_hv::MAX_VCPUS)
}

/// The host's CPUs, up to [`DEFAULT_MAX_CPUS`] (and what the hypervisor supports).
fn default_cpus() -> u32 {
    let host = std::thread::available_parallelism().map_or(1, |n| n.get());
    (host as u32).clamp(1, DEFAULT_MAX_CPUS.min(max_cpus()))
}

/// Checks that a VM can have `memory` bytes of RAM.
pub fn validate_memory(memory: u64) -> Result<()> {
    if !memory.is_multiple_of(4096) || memory < (64 << 20) {
        bail!("guest memory must be a multiple of 4 KiB and at least 64 MiB");
    }
    if memory > platform::MAX_LOW_RAM {
        bail!(
            "guest memory above {:#x} is not supported yet",
            platform::MAX_LOW_RAM
        );
    }
    Ok(())
}

/// Checks that midgard can run the VM (with `cpus` vCPUs).
fn validate(config: &MachineConfig, cpus: u32) -> Result<()> {
    if cpus == 0 || cpus > max_cpus() {
        bail!("the number of vCPUs must be between 1 and {}", max_cpus());
    }
    validate_memory(config.memory)?;
    if config.agent.is_some() && config.initrd.is_some() {
        bail!("the agent and an initrd are mutually exclusive");
    }
    if let Some(init) = &config.guest.init {
        if config.agent.is_none() {
            bail!("the distribution's init needs the agent");
        }
        if !init.starts_with('/') {
            bail!("the distribution's init must be an absolute path (not {init:?})");
        }
    } else if config.guest.ready != Ready::Immediately {
        bail!("waiting for the guest to boot needs the distribution's init");
    }
    if config.waypipe.is_some() && config.agent.is_none() {
        bail!("the guest's waypipe needs the agent");
    }
    Ok(())
}

/// The VM's virtio devices, and handles to those that have a host side.
struct Devices {
    list: DeviceList,
    /// The console (which takes the console's input once the VM exists).
    console: Option<DeviceRef<Console>>,
    vsock: Option<DeviceRef<Vsock>>,
    /// The virtio-net device and the frames for the network stack (which starts once
    /// the VM exists).
    network: Option<(DeviceRef<VirtioNet>, async_channel::Receiver<Vec<u8>>)>,
    /// The block devices and their host side (which starts once the VM exists).
    disks: Vec<(DeviceRef<Block>, block::Backend)>,
    /// The shared directory's device and its host side (which starts once the VM exists).
    share: Option<(DeviceRef<VirtioFs>, fs::Backend)>,
}

/// The devices of the VM that `config` describes; the console's output goes to
/// `output`.
fn create_devices(config: &MachineConfig, output: &ConsoleOutput) -> Result<Devices> {
    let mut list = DeviceList::default();
    let console =
        (config.console == ConsoleKind::Virtio).then(|| list.add(Console::new(output.clone())));
    // The agent talks to us over vsock.
    let vsock = config.agent.is_some().then(|| list.add(Vsock::default()));
    let network = config.network.then(|| {
        let (handle, from_guest) = net::channel();
        (list.add(VirtioNet::new(net::GUEST_MAC, handle)), from_guest)
    });
    let share = match &config.share {
        Some(directory) => {
            let (device, backend) = VirtioFs::new(SHARE_TAG, directory)
                .with_context(|| format!("cannot share {directory:?}"))?;
            Some((list.add(device), backend))
        }
        None => None,
    };
    let mut disks = Vec::new();
    for (index, disk) in config.disks.iter().enumerate() {
        let image = image::open(&disk.path, disk.read_only)
            .with_context(|| format!("failed to open disk {:?}", disk.path))?;
        let (block, backend) = Block::new(image, disk.read_only, index);
        disks.push((list.add(block), backend));
    }
    // Each device needs an interrupt line.
    if list.len() > platform::VIRTIO_IRQS.len() {
        bail!(
            "too many devices (a VM can have at most {} virtio devices)",
            platform::VIRTIO_IRQS.len()
        );
    }
    Ok(Devices {
        list,
        console,
        vsock,
        network,
        disks,
        share,
    })
}

/// The guest kernel's command line.
fn kernel_cmdline(config: &MachineConfig, devices: &Devices) -> String {
    let base = config.cmdline.as_deref().unwrap_or(BASE_CMDLINE);
    let console = match config.console {
        ConsoleKind::Virtio => "console=hvc0",
        ConsoleKind::Serial => "console=ttyS0",
    };
    let mut cmdline = vec![base.to_string(), console.to_string()];
    for slot in 0..devices.list.len() {
        let (base, irq) = platform::virtio_slot(slot);
        cmdline.push(format!(
            "virtio_mmio.device={}K@{base:#x}:{irq}",
            virtio::MMIO_SIZE >> 10
        ));
    }
    if devices.network.is_some() {
        cmdline.push(net::kernel_parameter());
    }
    let explicit_root = config
        .cmdline
        .iter()
        .chain(&config.append)
        .any(|parameters| {
            parameters
                .split_whitespace()
                .any(|p| p.starts_with("root="))
        });
    if let Some(disk) = config.disks.first()
        && !explicit_root
    {
        if config.agent.is_some() {
            // midgard-init mounts it.
            cmdline.push(format!("{}=/dev/vda", param::ROOT));
        } else if config.initrd.is_none() {
            // Without an initrd, the kernel mounts the first disk (vda) as the root file
            // system.
            let flags = if disk.read_only { "ro" } else { "rw" };
            cmdline.push(format!("root=/dev/vda {flags}"));
        }
    }
    cmdline.extend(config.append.iter().cloned());
    cmdline.join(" ")
}

fn read(path: &Path, what: &str) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to read {what} {path:?}"))
}

/// Reads the guest kernel and builds or reads the initrd.
fn load_images(config: &MachineConfig) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let kernel = read(&config.kernel, "the kernel")?;
    let initrd = match (&config.agent, &config.initrd) {
        (Some(agent), _) => {
            let guest_config = midgard_protocol::guest::encode_config(&config.guest)?;
            let waypipe = match &config.waypipe {
                Some(path) => Some(read(path, "the guest's waypipe")?),
                None => None,
            };
            Some(boot::initramfs::build(
                &read(agent, "the agent")?,
                &guest_config,
                waypipe.as_deref(),
            ))
        }
        (None, Some(path)) => Some(read(path, "the initrd")?),
        (None, None) => None,
    };
    Ok((kernel, initrd))
}

/// A running VM, owned by the thread that drives it.
///
/// The vCPUs run on their own threads; the virtio devices' queues, the host side of the
/// devices (e.g., disk I/O) and the tasks of [`MachineHandle::spawn`] run on an executor
/// that only this thread runs (see [`Machine::run`]). Dropping the `Machine` stops the VM (if it still
/// runs) and releases it. To interact with the VM from elsewhere, take a
/// [`MachineHandle`].
pub struct Machine {
    board: Arc<Board>,
    threads: Vec<JoinHandle<()>>,
    /// Receives the result of each vCPU that stops (the VM is done with the first one).
    stopped: async_channel::Receiver<Result<Stop>>,
    /// The first vCPU's result, once received (the error as text, such that it can be
    /// returned more than once).
    stop: OnceCell<Result<Stop, String>>,
    executor: Arc<Executor<'static>>,
    running: Arc<Running>,
    /// The block devices' I/O, which must finish before the VM is gone.
    disks: Vec<block::Shutdown>,
    /// The shared directory's requests, which must finish before the VM is gone.
    share: Option<fs::Shutdown>,
}

/// Counts the vCPU threads that did not exit yet.
#[derive(Default)]
struct Running {
    threads: AtomicUsize,
    /// Notified when a thread exits.
    exited: Event,
}

impl Running {
    /// Waits until all vCPU threads exited.
    async fn wait(&self) {
        loop {
            // Check again after listening, such that we do not miss the notification.
            let listener = self.exited.listen();
            if self.threads.load(Ordering::SeqCst) == 0 {
                return;
            }
            listener.await;
        }
    }

    /// Counts a vCPU thread until the returned guard is dropped (when the thread exits,
    /// also by panicking, or if it cannot be spawned).
    fn enter(self: &Arc<Self>) -> Exit {
        self.threads.fetch_add(1, Ordering::SeqCst);
        Exit(self.clone())
    }
}

/// See [`Running::enter`].
struct Exit(Arc<Running>);

impl Drop for Exit {
    fn drop(&mut self) {
        self.0.threads.fetch_sub(1, Ordering::SeqCst);
        self.0.exited.notify(usize::MAX);
    }
}

impl Machine {
    /// A handle for other threads and tasks.
    pub fn handle(&self) -> MachineHandle {
        MachineHandle {
            board: Arc::downgrade(&self.board),
            executor: Arc::downgrade(&self.executor),
        }
    }

    /// Runs the VM's tasks on this thread until `future` completes.
    pub fn run<T>(&self, future: impl Future<Output = T>) -> T {
        async_io::block_on(self.executor.run(future))
    }

    /// Runs the VM's tasks on this thread until the guest stops (or a vCPU fails);
    /// returns why.
    pub fn run_until_stopped(&self) -> Result<Stop> {
        if self.stop.get().is_none() {
            // Each vCPU thread sends its result before it exits.
            let result = self
                .run(self.stopped.recv())
                .expect("all vCPU threads exited");
            self.record(result);
        }
        self.try_stopped().expect("the stop was recorded")
    }

    /// Whether the guest stopped already (without running anything).
    pub fn try_stopped(&self) -> Option<Result<Stop>> {
        if self.stop.get().is_none()
            && let Ok(result) = self.stopped.try_recv()
        {
            self.record(result);
        }
        let stop = self.stop.get()?;
        Some(stop.clone().map_err(|error| anyhow!("{error}")))
    }

    fn record(&self, result: Result<Stop>) {
        let _ = self.stop.set(result.map_err(|error| format!("{error:#}")));
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.board.stop();
        let disks = std::mem::take(&mut self.disks);
        let share = self.share.take();
        async_io::block_on(self.executor.run(async {
            // The VM's tasks keep running until the vCPUs are gone: a vCPU may wait for
            // them (e.g., for disk I/O when the guest resets the device).
            self.running.wait().await;
            // The guest is gone, but its disk I/O must finish (e.g., before another VM
            // may open the images).
            for disk in disks {
                disk.wait().await;
            }
            if let Some(share) = share {
                share.wait().await;
            }
        }));
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// What other threads and tasks may do with a VM while it runs: stop it, talk to the
/// guest over vsock, and run tasks next to the VM's own.
///
/// Cheap to clone; usable from any thread. It does not keep the VM alive: once the
/// owner's [`Machine`] is gone, operations fail or do nothing.
#[derive(Clone)]
pub struct MachineHandle {
    board: Weak<Board>,
    executor: Weak<Executor<'static>>,
}

impl MachineHandle {
    fn board(&self) -> io::Result<Arc<Board>> {
        self.board
            .upgrade()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "the VM is gone"))
    }

    /// Makes the guest stop with [`Stop::Requested`].
    pub fn stop(&self) {
        if let Ok(board) = self.board() {
            board.stop();
        }
    }

    /// Runs a task on the VM's executor (e.g., a session that forwards bytes to the
    /// guest). The task runs while the owner drives the VM; it is dropped with the VM.
    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        if let Some(executor) = self.executor.upgrade() {
            executor.spawn(future).detach();
        }
    }

    /// Connects to a vsock port that the guest listens on.
    pub async fn vsock_connect(&self, port: u32) -> io::Result<VsockStream> {
        VsockStream::connect(&self.board()?, port).await
    }

    /// The counters of the vCPUs and of the devices, for diagnostics. Blocks (e.g., while
    /// a device works on its queues), so not for the VM's executor.
    pub fn counters(&self) -> Result<Counters> {
        self.board()?.counters()
    }

    /// Listens on a host vsock port for connections from the guest.
    pub fn vsock_listen(&self, port: u32) -> io::Result<VsockListener> {
        VsockListener::bind(&self.board()?, port)
    }
}

/// Why a VM stopped. `Display` gives the sentence for the user ("the guest powered
/// off").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The guest requested a reset (keyboard controller, port 0xCF9 or a triple fault).
    Reset,
    /// The guest entered S5 (ACPI poweroff).
    PowerOff,
    /// We stopped the VM (see [`MachineHandle::stop`]).
    Requested,
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Stop::Reset => "the guest reset the machine",
            Stop::PowerOff => "the guest powered off",
            Stop::Requested => "the VM was stopped",
        })
    }
}

/// Starts a thread for each vCPU. The VM stops as soon as one vCPU stops (e.g., because
/// the guest reset it): that one stops the others, after it sent its result.
fn spawn_vcpus(
    board: Arc<Board>,
    executor: Arc<Executor<'static>>,
    disks: Vec<block::Shutdown>,
    share: Option<fs::Shutdown>,
    vcpus: Vec<vcpu::Vcpu>,
) -> Result<Machine> {
    let (sender, stopped) = async_channel::unbounded();
    // If a thread cannot be spawned, dropping the machine stops the others.
    let mut machine = Machine {
        board,
        threads: Vec::new(),
        stopped,
        stop: OnceCell::new(),
        executor,
        running: Arc::default(),
        disks,
        share,
    };
    for (id, mut vcpu) in vcpus.into_iter().enumerate() {
        let sender = sender.clone();
        let board = machine.board.clone();
        let exit = machine.running.enter();
        let thread = std::thread::Builder::new()
            .name(format!("vcpu{id}"))
            .spawn(move || {
                let _exit = exit;
                // A panic stops the VM like an error does (the panic hook printed it).
                let result = panic::catch_unwind(AssertUnwindSafe(|| vcpu.run()))
                    .unwrap_or_else(|_| Err(anyhow!("vCPU {id} panicked")));
                let _ = sender.send_blocking(result);
                board.stop();
            })
            .context("failed to spawn a vCPU thread")?;
        machine.threads.push(thread);
    }
    Ok(machine)
}

/// Boots the VM that `config` describes. The guest's console output (the serial port
/// and virtio-console) goes to `console`, the writing side of a pipe; the guest does not
/// wait for its reader (output that does not fit is dropped). The pipe closes once the
/// `Machine` is dropped. `input` (if any) goes to the guest's console as fast as the
/// guest reads it.
///
/// The vCPUs start running on their own threads before this returns; the caller must
/// then drive the VM with [`Machine::run`] or [`Machine::run_until_stopped`].
pub fn start(
    config: &MachineConfig,
    console: piper::Writer,
    input: Option<async_channel::Receiver<Vec<u8>>>,
) -> Result<Machine> {
    let cpus = config.cpus.unwrap_or_else(default_cpus);
    validate(config, cpus)?;
    let console = ConsoleOutput::new(console);
    let devices = create_devices(config, &console)?;
    let cmdline = kernel_cmdline(config, &devices);
    let (kernel, initrd) = load_images(config)?;

    let (hypervisor, memory) = midgard_hv::create(config.memory)?;
    let tsc_hz = hypervisor.tsc_hz()?;
    log!("TSC runs at {} kHz", tsc_hz / 1000);
    let entry = boot::load(&memory, &kernel, initrd.as_deref(), &cmdline)?;
    drop((kernel, initrd));
    acpi::write(&memory, platform::ACPI_TABLES, cpus).context("failed to write the ACPI tables")?;

    let board = Arc::new(Board::new(
        memory,
        cpus,
        vcpu::Timebase::new(tsc_hz),
        hypervisor,
        console,
        devices.list.into_transports(),
        devices.vsock,
    ));
    let mut vcpus = Vec::new();
    for id in 0..cpus {
        vcpus.push(vcpu::Vcpu::new(board.clone(), id)?);
    }
    vcpus[0].set_registers(&boot::entry_registers(&entry))?;

    log!(
        "booting {} with {} vCPU(s) and {} MiB of memory: {cmdline}",
        config.kernel.display(),
        cpus,
        config.memory >> 20,
    );
    let executor = Arc::new(Executor::new());
    for slot in 0..board.num_virtio() {
        executor.spawn(board.clone().process_virtio(slot)).detach();
    }
    if let Some(input) = input {
        match devices.console {
            Some(device) => console::start(&executor, board.clone(), device, input),
            None => executor.spawn(board.clone().feed_serial(input)).detach(),
        }
    }
    if let Some((device, from_guest)) = devices.network {
        let board = board.clone();
        net::spawn(&executor, from_guest, move |frames| {
            board.with_device(device, |net, queues, memory| {
                net.deliver(frames, queues, memory)
            });
        });
    }
    let disks = (devices.disks.into_iter())
        .map(|(device, backend)| block::start(&executor, board.clone(), device, backend))
        .collect();
    let share = (devices.share)
        .map(|(device, backend)| fs::start(&executor, board.clone(), device, backend));
    spawn_vcpus(board, executor, disks, share, vcpus)
}
