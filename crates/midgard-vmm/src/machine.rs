//! Setting up, starting and stopping a VM.

use std::fmt;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use async_executor::Executor;
use event_listener::Event;
use midgard_protocol::machine::MachineConfig;

use crate::board::Board;
use crate::logging::log;
use crate::{acpi, boot, clock, platform, vcpu};

/// Kernel parameters that every guest needs (unless the configuration replaces them);
/// the console is added.
const BASE_CMDLINE: &str = "pci=off reboot=k panic=-1";

/// Checks that midgard can run the VM.
fn validate(config: &MachineConfig) -> Result<()> {
    if config.cpus == 0 || config.cpus > platform::MAX_CPUS {
        bail!(
            "the number of vCPUs must be between 1 and {}",
            platform::MAX_CPUS
        );
    }
    if !config.memory.is_multiple_of(4096) || config.memory < (64 << 20) {
        bail!("guest memory must be a multiple of 4 KiB and at least 64 MiB");
    }
    if config.memory > platform::MAX_LOW_RAM {
        bail!(
            "guest memory above {:#x} is not supported yet",
            platform::MAX_LOW_RAM
        );
    }
    if !config.disks.is_empty() {
        bail!("disks are not supported yet");
    }
    Ok(())
}

/// The guest kernel's command line.
fn kernel_cmdline(config: &MachineConfig) -> String {
    let base = config.cmdline.as_deref().unwrap_or(BASE_CMDLINE);
    let mut cmdline = vec![base.to_string(), "console=ttyS0".to_string()];
    cmdline.extend(config.append.iter().cloned());
    cmdline.join(" ")
}

fn read(path: &Path, what: &str) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to read {what} {path:?}"))
}

/// A running VM, owned by the thread that drives it.
///
/// The vCPUs run on their own threads; the host side of the devices (e.g., disk I/O)
/// runs as tasks on an executor that only this thread runs (see
/// [`Machine::run_until_stopped`]). Dropping the `Machine` stops the VM (if it still
/// runs) and releases it. To interact with the VM from elsewhere, take a
/// [`MachineHandle`].
pub struct Machine {
    board: Arc<Board>,
    threads: Vec<JoinHandle<()>>,
    /// Receives the result of each vCPU that stops (the VM is done with the first one).
    stopped: async_channel::Receiver<Result<Stop>>,
    executor: Arc<Executor<'static>>,
    running: Arc<Running>,
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
    /// A handle for other threads.
    pub fn handle(&self) -> MachineHandle {
        MachineHandle {
            board: Arc::downgrade(&self.board),
        }
    }

    /// Runs the VM's tasks on this thread until the guest stops (or a vCPU fails);
    /// returns why. Flushes the console before returning.
    pub fn run_until_stopped(&mut self) -> Result<Stop> {
        // Each vCPU thread sends its result before it exits.
        let result = async_io::block_on(self.executor.run(self.stopped.recv()))
            .expect("all vCPU threads exited");
        self.board.flush_console();
        result
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.board.stop();
        // The VM's tasks keep running until the vCPUs are gone: a vCPU may wait for them
        // (e.g., for a device's host side).
        async_io::block_on(self.executor.run(self.running.wait()));
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        self.board.flush_console();
    }
}

/// What other threads may do with a VM while it runs: stop it and flush its console.
///
/// Cheap to clone; usable from any thread. It does not keep the VM alive: once the
/// owner's [`Machine`] is gone, operations do nothing.
#[derive(Clone)]
pub struct MachineHandle {
    board: Weak<Board>,
}

impl MachineHandle {
    /// Makes the guest stop with [`Stop::Requested`].
    pub fn stop(&self) {
        if let Some(board) = self.board.upgrade() {
            board.stop();
        }
    }

    /// Flushes buffered console output to the sink given to [`start`].
    pub fn flush_console(&self) {
        if let Some(board) = self.board.upgrade() {
            board.flush_console();
        }
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
    vcpus: Vec<vcpu::Vcpu>,
) -> Result<Machine> {
    let (sender, stopped) = async_channel::unbounded();
    // If a thread cannot be spawned, dropping the machine stops the others.
    let mut machine = Machine {
        board,
        threads: Vec::new(),
        stopped,
        executor,
        running: Arc::default(),
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

/// Boots the VM that `config` describes. The guest's console output (the serial port)
/// goes to `console`. The vCPUs start running on their own threads before this returns;
/// the caller must then drive the VM with [`Machine::run_until_stopped`].
pub fn start(config: &MachineConfig, console: Box<dyn Write + Send>) -> Result<Machine> {
    validate(config)?;
    let cmdline = kernel_cmdline(config);
    let kernel = read(&config.kernel, "the kernel")?;
    let initrd = match &config.initrd {
        Some(path) => Some(read(path, "the initrd")?),
        None => None,
    };

    let tsc_hz = clock::calibrate();
    log!("TSC runs at {} kHz", tsc_hz / 1000);

    let (hypervisor, memory) = midgard_hv::create(config.memory)?;
    let entry = boot::load(&memory, &kernel, initrd.as_deref(), &cmdline)?;
    drop((kernel, initrd));
    acpi::write(&memory, platform::ACPI_TABLES, config.cpus)
        .context("failed to write the ACPI tables")?;

    let board = Arc::new(Board::new(memory, config.cpus, tsc_hz, hypervisor, console));
    let mut vcpus = Vec::new();
    for id in 0..config.cpus {
        vcpus.push(vcpu::Vcpu::new(board.clone(), id)?);
    }
    vcpus[0].set_registers(&boot::entry_registers(&entry))?;

    log!(
        "booting {} with {} vCPU(s) and {} MiB of memory: {cmdline}",
        config.kernel.display(),
        config.cpus,
        config.memory >> 20,
    );
    let executor = Arc::new(Executor::new());
    spawn_vcpus(board, executor, vcpus)
}
