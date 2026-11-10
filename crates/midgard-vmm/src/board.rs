//! The guest's hardware (see [`Board`]).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use midgard_hv::{GuestMemory, Hypervisor, Kicker};

use crate::devices::pm1::{self, Pm1};
use crate::devices::serial::{self, Serial};
use crate::devices::{read_bytes, write_bytes};
use crate::logging::{ReportOnce, log};
use crate::machine::Stop;
use crate::virtio::vsock::Vsock;
use crate::virtio::{self, DeviceRef, Effect, Transport};
use crate::{platform, vcpu};

/// Problems that are reported once (see [`ReportOnce`]).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Unhandled {
    /// Setting an interrupt line failed.
    IrqLine(usize),
    Port(u16),
    Mmio(u64),
}

/// The guest's hardware, shared by all vCPUs: RAM, the devices that are not in the
/// hypervisor (UART, PM1, the virtio transports) and the routing of port I/O, MMIO and
/// interrupt lines to them. The local APICs and the I/O APIC are the hypervisor's. Also
/// tracks whether the guest is done (stop requested).
///
/// vCPU threads call into it on every exit. The virtio devices' queues are processed by
/// tasks on the VM's executor (see [`Board::process_virtio`]); the host side of virtio
/// devices reaches its device through [`Board::with_device`] (or [`Board::with_vsock`]).
pub(crate) struct Board {
    hypervisor: Hypervisor,
    pub(crate) memory: GuestMemory,
    pub(crate) num_cpus: u32,
    /// The TSC frequency in Hz.
    pub(crate) tsc_hz: u64,
    serial: Mutex<Serial>,
    /// The level of the serial port's interrupt line.
    serial_irq: AtomicBool,
    pm1: Mutex<Pm1>,
    /// The vCPUs' kickers (see [`Board::stop`]).
    kickers: Mutex<Vec<Kicker>>,
    /// [`Board::stop`] was called.
    stopping: AtomicBool,
    /// The virtio devices, in the order of their slots (see [`platform::virtio_slot`]).
    virtio: Vec<Transport>,
    vsock: Option<DeviceRef<Vsock>>,
    reported: ReportOnce<Unhandled>,
    /// What the vCPUs reported (each vCPU tends to do the same, e.g., probe an MSR).
    pub(crate) vcpu_reported: ReportOnce<vcpu::Unsupported>,
}

impl Board {
    /// The serial port's output goes to `console`.
    pub(crate) fn new(
        memory: GuestMemory,
        num_cpus: u32,
        tsc_hz: u64,
        hypervisor: Hypervisor,
        console: Box<dyn Write + Send>,
        virtio: Vec<Transport>,
        vsock: Option<DeviceRef<Vsock>>,
    ) -> Self {
        Self {
            hypervisor,
            memory,
            num_cpus,
            tsc_hz,
            serial: Mutex::new(Serial::new(console)),
            serial_irq: AtomicBool::new(false),
            pm1: Mutex::new(Pm1::default()),
            kickers: Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
            virtio,
            vsock,
            reported: ReportOnce::new(),
            vcpu_reported: ReportOnce::new(),
        }
    }

    /// The VM in the hypervisor (e.g., for creating vCPUs).
    pub(crate) fn hypervisor(&self) -> &Hypervisor {
        &self.hypervisor
    }

    /// Lets [`Board::stop`] kick a vCPU.
    pub(crate) fn add_kicker(&self, kicker: Kicker) {
        self.kickers.lock().unwrap().push(kicker);
    }

    /// Makes all vCPUs stop with [`Stop::Requested`] (from any thread).
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        for kicker in self.kickers.lock().unwrap().iter() {
            kicker.kick();
        }
    }

    /// Whether [`Board::stop`] was called.
    pub(crate) fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    fn set_irq_line(&self, pin: usize, level: bool) {
        if let Err(error) = self.hypervisor.set_irq_line(pin as u32, level)
            && self.reported.first(Unhandled::IrqLine(pin))
        {
            log!("{error:#}");
        }
    }

    /// Sets the level of the serial port's (level-triggered) interrupt line.
    fn set_serial_irq(&self, level: bool) {
        if self.serial_irq.swap(level, Ordering::SeqCst) != level {
            self.set_irq_line(platform::COM1_IRQ, level);
        }
    }

    /// Generates an edge on an (ISA, edge-triggered) interrupt line.
    fn pulse_irq(&self, pin: usize) {
        self.set_irq_line(pin, true);
        self.set_irq_line(pin, false);
    }

    pub(crate) fn flush_console(&self) {
        self.serial.lock().unwrap().flush();
    }

    /// Runs `f` on a virtio device (for the host side of devices); raises its interrupt
    /// if `f` added used buffers.
    pub(crate) fn with_device<D: virtio::Device, R>(
        &self,
        device: DeviceRef<D>,
        f: impl FnOnce(&mut D, &mut [virtio::Queue], &GuestMemory) -> (R, bool),
    ) -> R {
        let transport = &self.virtio[device.slot()];
        transport.with_device(&self.memory, f, || self.pulse_irq(transport.irq))
    }

    /// The number of virtio devices.
    pub(crate) fn num_virtio(&self) -> usize {
        self.virtio.len()
    }

    /// Processes the queues of the virtio device in `slot` when the guest notifies them
    /// (a task on the VM's executor, which runs as long as the VM does).
    pub(crate) async fn process_virtio(self: Arc<Self>, slot: usize) {
        let transport = &self.virtio[slot];
        (transport.process_notified(&self.memory, || self.pulse_irq(transport.irq))).await;
    }

    /// Runs `f` on the vsock device; returns None without one.
    pub(crate) fn with_vsock<R>(
        &self,
        f: impl FnOnce(&mut Vsock, &mut [virtio::Queue], &GuestMemory) -> (R, bool),
    ) -> Option<R> {
        Some(self.with_device(self.vsock?, f))
    }

    fn report(&self, key: Unhandled, what: std::fmt::Arguments) {
        if self.reported.first(key) {
            log!("unhandled {what} (further accesses are not reported)");
        }
    }

    /// Port input of `size` bytes.
    pub(crate) fn pio_in(&self, port: u16, size: u32) -> u32 {
        if let Some(offset) = port_offset(port, platform::COM1_PORT, serial::NUM_PORTS) {
            let mut serial = self.serial.lock().unwrap();
            let value = read_bytes(serial::NUM_PORTS, offset, size, |o| serial.read(o));
            self.set_serial_irq(serial.irq_level());
            return value;
        }
        if let Some(offset) = port_offset(port, platform::PM1A_EVT_BLK, pm1::EVT_LEN) {
            let pm1 = self.pm1.lock().unwrap();
            return read_bytes(pm1::EVT_LEN, offset, size, |o| pm1.read_event(o));
        }
        if let Some(offset) = port_offset(port, platform::PM1A_CNT_BLK, pm1::CNT_LEN) {
            let pm1 = self.pm1.lock().unwrap();
            return read_bytes(pm1::CNT_LEN, offset, size, |o| pm1.read_control(o));
        }
        let ones = if size >= 4 {
            u32::MAX
        } else {
            (1u32 << (8 * size)) - 1
        };
        match port {
            // There is no 8259 PIC; Linux detects that from a mask register that
            // does not read back as written.
            0x20 | 0x21 | 0xA0 | 0xA1 => 0,
            // No keyboard controller, PIT, DMA controller or COM2-4; avoid the noise
            // from Linux probing them.
            0x60 | 0x64 | 0x40..=0x43 | 0x61 | 0x80..=0x8F => ones,
            0x2F8..=0x2FF | 0x3E8..=0x3EF | 0x2E8..=0x2EF => ones,
            // No PCI configuration space (Linux probes it early, despite pci=off).
            0xCF8..=0xCFF => ones,
            _ => {
                self.report(
                    Unhandled::Port(port),
                    format_args!("port read at {port:#x}"),
                );
                ones
            }
        }
    }

    /// Port output of `size` bytes.
    pub(crate) fn pio_out(&self, port: u16, size: u32, value: u32) -> Option<Stop> {
        if let Some(offset) = port_offset(port, platform::COM1_PORT, serial::NUM_PORTS) {
            let mut serial = self.serial.lock().unwrap();
            write_bytes(serial::NUM_PORTS, offset, size, value, |o, byte| {
                serial.write(o, byte)
            });
            self.set_serial_irq(serial.irq_level());
            return None;
        }
        if let Some(offset) = port_offset(port, platform::PM1A_EVT_BLK, pm1::EVT_LEN) {
            let mut pm1 = self.pm1.lock().unwrap();
            write_bytes(pm1::EVT_LEN, offset, size, value, |o, byte| {
                pm1.write_event(o, byte)
            });
            return None;
        }
        if let Some(offset) = port_offset(port, platform::PM1A_CNT_BLK, pm1::CNT_LEN) {
            let mut pm1 = self.pm1.lock().unwrap();
            let mut poweroff = false;
            write_bytes(pm1::CNT_LEN, offset, size, value, |o, byte| {
                poweroff |= pm1.write_control(o, byte)
            });
            return poweroff.then_some(Stop::PowerOff);
        }
        match port {
            // Keyboard controller: pulse the reset line.
            0x64 if value as u8 == 0xFE => return Some(Stop::Reset),
            // Reset control register: bit 2 starts the reset.
            0xCF9 if value & 0x4 != 0 => return Some(Stop::Reset),
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x60 | 0x64 | 0x40..=0x43 | 0x61 | 0x80..=0x8F => {}
            0x2F8..=0x2FF | 0x3E8..=0x3EF | 0x2E8..=0x2EF | 0xCF8..=0xCFF => {}
            _ => self.report(
                Unhandled::Port(port),
                format_args!("port write at {port:#x}"),
            ),
        }
        None
    }

    /// The virtio device whose registers are at `address`, and the offset into them.
    fn virtio_device(&self, address: u64) -> Option<(usize, u64)> {
        let offset = address.checked_sub(platform::VIRTIO_BASE)?;
        let index = (offset / virtio::MMIO_SIZE) as usize;
        (index < self.virtio.len()).then_some((index, offset % virtio::MMIO_SIZE))
    }

    /// MMIO read of `size` bytes.
    pub(crate) fn mmio_read(&self, address: u64, size: u8) -> u64 {
        if let Some((index, offset)) = self.virtio_device(address) {
            return self.virtio[index].read(offset, size);
        }
        self.report(
            Unhandled::Mmio(address),
            format_args!("MMIO read of {size} bytes at {address:#x}"),
        );
        u64::MAX
    }

    /// MMIO write of `size` bytes.
    pub(crate) fn mmio_write(&self, address: u64, size: u8, value: u64) {
        if let Some((index, offset)) = self.virtio_device(address) {
            let transport = &self.virtio[index];
            match transport.write(offset, size, value) {
                Effect::None => {}
                Effect::Reset(in_flight) => {
                    // The requests complete on the VM's executor, which keeps running
                    // while vCPUs exist (see Machine).
                    in_flight.wait();
                    transport.finish_reset();
                }
            }
            return;
        }
        self.report(
            Unhandled::Mmio(address),
            format_args!("MMIO write of {size} bytes at {address:#x}"),
        );
    }
}

/// The offset of `port` in a device with `len` ports at `base`.
fn port_offset(port: u16, base: u16, len: u16) -> Option<u16> {
    port.checked_sub(base).filter(|&offset| offset < len)
}
