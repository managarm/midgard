//! The guest's hardware (see [`Board`]).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use event_listener::Event;
use midgard_hv::{Counters, GuestMemory, Hypervisor, Kicker};
use midgard_protocol::control;

use crate::counters::{Accesses, Counter};
use crate::devices::pm1::{self, Pm1};
use crate::devices::rtc::{self, Rtc};
use crate::devices::serial::{self, Serial};
use crate::devices::{read_bytes, write_bytes};
use crate::host::console::ConsoleOutput;
use crate::logging::{ReportOnce, log};
use crate::machine::Stop;
use crate::virtio::vsock::Vsock;
use crate::virtio::{self, DeviceRef, Effect, Transport};
use crate::{pci, platform, vcpu};

/// Problems that are reported once (see [`ReportOnce`]).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Unhandled {
    /// Setting an interrupt line failed.
    IrqLine(usize),
    /// Sending an MSI failed.
    Msi,
    Port(u16),
    Mmio(u64),
}

/// The number of interrupt lines whose edges the board counts (the ISA IRQs).
const NUM_COUNTED_IRQS: usize = 16;

/// What the board counts (see [`Board::counters`]); the virtio transports count for
/// themselves.
#[derive(Default)]
struct DeviceCounters {
    serial: Accesses,
    rtc: Accesses,
    pm1: Accesses,
    /// The PCI bus' configuration space.
    pci: Accesses,
    /// The ports of devices that we do not have, which we ignore on purpose.
    ignored: Accesses,
    unhandled_pio: Accesses,
    unhandled_mmio: Accesses,
    /// The rising edges of each interrupt line.
    irqs: [Counter; NUM_COUNTED_IRQS],
}

/// The guest's hardware, shared by all vCPUs: RAM, the devices that are not in the
/// hypervisor (UART, RTC, PM1, the PCI bus with the virtio devices) and the routing of
/// port I/O, MMIO and interrupts to them. The local APICs and the I/O APIC are the
/// hypervisor's. Also tracks whether the guest is done (stop requested).
///
/// vCPU threads call into it on every exit. The virtio devices' queues are processed by
/// tasks on the VM's executor (see [`Board::process_virtio`]); the host side of virtio
/// devices reaches its device through [`Board::with_device`] (or [`Board::with_vsock`]).
pub(crate) struct Board {
    hypervisor: Hypervisor,
    pub(crate) memory: GuestMemory,
    pub(crate) num_cpus: u32,
    /// What kvmclock tells the guest.
    pub(crate) timebase: vcpu::Timebase,
    serial: Mutex<Serial>,
    /// The level of the serial port's interrupt line.
    serial_irq: AtomicBool,
    /// Notified when the serial port's receive FIFO is empty (see
    /// [`Board::feed_serial`]).
    serial_drained: Event,
    rtc: Mutex<Rtc>,
    pm1: Mutex<Pm1>,
    pci: pci::Bus,
    /// The vCPUs' kickers (see [`Board::stop`]).
    kickers: Mutex<Vec<Kicker>>,
    /// The vCPUs' counters, by vCPU ID (see [`Board::counters`]).
    vcpu_counters: Mutex<BTreeMap<u32, Counters>>,
    device_counters: DeviceCounters,
    /// [`Board::stop`] was called.
    stopping: AtomicBool,
    /// The virtio devices, in the order of their PCI slots (see [`platform::virtio_slot`]).
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
        timebase: vcpu::Timebase,
        hypervisor: Hypervisor,
        console: ConsoleOutput,
        virtio: Vec<Transport>,
        vsock: Option<DeviceRef<Vsock>>,
    ) -> Self {
        let mut pci = pci::Bus::new();
        for (index, transport) in virtio.iter().enumerate() {
            let (slot, bar) = platform::virtio_slot(index);
            pci.add(slot, transport.config_space(bar as u32));
        }
        Self {
            hypervisor,
            memory,
            num_cpus,
            timebase,
            serial: Mutex::new(Serial::new(console)),
            serial_irq: AtomicBool::new(false),
            serial_drained: Event::new(),
            rtc: Mutex::new(Rtc::default()),
            pm1: Mutex::new(Pm1::default()),
            pci,
            kickers: Mutex::new(Vec::new()),
            vcpu_counters: Mutex::new(BTreeMap::new()),
            device_counters: DeviceCounters::default(),
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

    /// Lets [`Board::counters`] read the counters of vCPU `id`.
    pub(crate) fn add_vcpu_counters(&self, id: u32, counters: Counters) {
        self.vcpu_counters.lock().unwrap().insert(id, counters);
    }

    /// The vCPUs' and the devices' counters.
    pub(crate) fn counters(&self) -> Result<control::Counters> {
        let vcpu_counters = self.vcpu_counters.lock().unwrap().clone();
        let vcpus = vcpu_counters
            .values()
            .map(|counters| Ok(counters.snapshot()?.into_iter().collect()))
            .collect::<Result<_>>()?;
        let counters = &self.device_counters;
        let mut devices = BTreeMap::new();
        counters.serial.snapshot("serial", "pio", &mut devices);
        counters.rtc.snapshot("rtc", "pio", &mut devices);
        counters.pm1.snapshot("pm1", "pio", &mut devices);
        counters.pci.snapshot("pci", "ecam", &mut devices);
        counters.ignored.snapshot("ignored", "pio", &mut devices);
        counters
            .unhandled_pio
            .snapshot("unhandled", "pio", &mut devices);
        counters
            .unhandled_mmio
            .snapshot("unhandled", "mmio", &mut devices);
        let serial_irqs = counters
            .irqs
            .get(platform::COM1_IRQ)
            .map_or(0, Counter::get);
        devices.insert("serial.irqs".into(), serial_irqs);
        // Devices of the same kind are numbered in the order of their slots.
        let mut kinds = BTreeMap::<&str, usize>::new();
        for transport in &self.virtio {
            let kind = transport.kind();
            let number = kinds.entry(kind).or_default();
            let name = format!("virtio-{kind}{number}");
            *number += 1;
            transport.snapshot(&name, &mut devices);
        }
        Ok(control::Counters { vcpus, devices })
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
        if level && let Some(counter) = self.device_counters.irqs.get(pin) {
            counter.add();
        }
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

    /// Sends a message signaled interrupt (of a PCI device).
    fn signal_msi(&self, address: u64, data: u32) {
        if let Err(error) = self.hypervisor.signal_msi(address, data)
            && self.reported.first(Unhandled::Msi)
        {
            log!("{error:#} (further failures are not reported)");
        }
    }

    /// Passes the console's input to the serial port, as fast as the guest reads it (a
    /// task on the VM's executor, which runs until the input ends).
    pub(crate) async fn feed_serial(self: Arc<Self>, input: async_channel::Receiver<Vec<u8>>) {
        let mut pending = VecDeque::new();
        while let Ok(data) = input.recv().await {
            pending.extend(data);
            while !pending.is_empty() {
                // Listen first, such that we do not miss the guest emptying the FIFO.
                let drained = self.serial_drained.listen();
                {
                    let mut serial = self.serial.lock().unwrap();
                    serial.push_input(&mut pending);
                    self.set_serial_irq(serial.irq_level());
                }
                if !pending.is_empty() {
                    drained.await;
                }
            }
        }
    }

    /// Notifies [`Board::feed_serial`] if the guest emptied the serial port's receive
    /// FIFO.
    fn check_serial_drained(&self, serial: &Serial) {
        if serial.input_empty() {
            self.serial_drained.notify(1);
        }
    }

    /// Runs `f` on a virtio device (for the host side of devices); interrupts the driver
    /// if `f` returned chains to it.
    pub(crate) fn with_device<D: virtio::Device, R>(
        &self,
        device: DeviceRef<D>,
        f: impl FnOnce(&mut D, &mut [virtio::Queue], &GuestMemory) -> R,
    ) -> R {
        let send = |address, data| self.signal_msi(address, data);
        self.virtio[device.slot()].with_device(&self.memory, f, send)
    }

    /// The number of virtio devices.
    pub(crate) fn num_virtio(&self) -> usize {
        self.virtio.len()
    }

    /// Processes the queues of the virtio device `index` when the guest notifies them (a
    /// task on the VM's executor, which runs as long as the VM does).
    pub(crate) async fn process_virtio(self: Arc<Self>, index: usize) {
        let send = |address, data| self.signal_msi(address, data);
        (self.virtio[index].process_notified(&self.memory, send)).await;
    }

    /// Runs `f` on the vsock device; returns None without one.
    pub(crate) fn with_vsock<R>(
        &self,
        f: impl FnOnce(&mut Vsock, &mut [virtio::Queue], &GuestMemory) -> R,
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
            self.device_counters.serial.reads.add();
            let mut serial = self.serial.lock().unwrap();
            let value = read_bytes(serial::NUM_PORTS, offset, size, |o| serial.read(o));
            self.set_serial_irq(serial.irq_level());
            self.check_serial_drained(&serial);
            return value;
        }
        if let Some(offset) = port_offset(port, platform::RTC_PORT, rtc::NUM_PORTS) {
            self.device_counters.rtc.reads.add();
            let rtc = self.rtc.lock().unwrap();
            return read_bytes(rtc::NUM_PORTS, offset, size, |o| rtc.read(o));
        }
        if let Some(offset) = port_offset(port, platform::PM1A_EVT_BLK, pm1::EVT_LEN) {
            self.device_counters.pm1.reads.add();
            let pm1 = self.pm1.lock().unwrap();
            return read_bytes(pm1::EVT_LEN, offset, size, |o| pm1.read_event(o));
        }
        if let Some(offset) = port_offset(port, platform::PM1A_CNT_BLK, pm1::CNT_LEN) {
            self.device_counters.pm1.reads.add();
            let pm1 = self.pm1.lock().unwrap();
            return read_bytes(pm1::CNT_LEN, offset, size, |o| pm1.read_control(o));
        }
        let ones = if size >= 4 {
            u32::MAX
        } else {
            (1u32 << (8 * size)) - 1
        };
        let value = match port {
            // There is no 8259 PIC; Linux detects that from a mask register that
            // does not read back as written.
            0x20 | 0x21 | 0xA0 | 0xA1 => 0,
            // No keyboard controller, PIT, DMA controller or COM2-4; avoid the noise
            // from Linux probing them.
            0x60 | 0x64 | 0x40..=0x43 | 0x61 | 0x80..=0x8F => ones,
            0x2F8..=0x2FF | 0x3E8..=0x3EF | 0x2E8..=0x2EF => ones,
            // No PCI configuration mechanism #1, such that Linux uses ECAM (see pci).
            0xCF8..=0xCFF => ones,
            _ => {
                self.device_counters.unhandled_pio.reads.add();
                self.report(
                    Unhandled::Port(port),
                    format_args!("port read at {port:#x}"),
                );
                return ones;
            }
        };
        self.device_counters.ignored.reads.add();
        value
    }

    /// Port output of `size` bytes.
    pub(crate) fn pio_out(&self, port: u16, size: u32, value: u32) -> Option<Stop> {
        if let Some(offset) = port_offset(port, platform::COM1_PORT, serial::NUM_PORTS) {
            self.device_counters.serial.writes.add();
            let mut serial = self.serial.lock().unwrap();
            write_bytes(serial::NUM_PORTS, offset, size, value, |o, byte| {
                serial.write(o, byte)
            });
            self.set_serial_irq(serial.irq_level());
            // E.g., the guest cleared the FIFO.
            self.check_serial_drained(&serial);
            return None;
        }
        if let Some(offset) = port_offset(port, platform::RTC_PORT, rtc::NUM_PORTS) {
            self.device_counters.rtc.writes.add();
            let mut rtc = self.rtc.lock().unwrap();
            write_bytes(rtc::NUM_PORTS, offset, size, value, |o, byte| {
                rtc.write(o, byte)
            });
            return None;
        }
        if let Some(offset) = port_offset(port, platform::PM1A_EVT_BLK, pm1::EVT_LEN) {
            self.device_counters.pm1.writes.add();
            let mut pm1 = self.pm1.lock().unwrap();
            write_bytes(pm1::EVT_LEN, offset, size, value, |o, byte| {
                pm1.write_event(o, byte)
            });
            return None;
        }
        if let Some(offset) = port_offset(port, platform::PM1A_CNT_BLK, pm1::CNT_LEN) {
            self.device_counters.pm1.writes.add();
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
            _ => {
                self.device_counters.unhandled_pio.writes.add();
                self.report(
                    Unhandled::Port(port),
                    format_args!("port write at {port:#x}"),
                );
                return None;
            }
        }
        self.device_counters.ignored.writes.add();
        None
    }

    /// The virtio device in a PCI slot.
    fn virtio_in_slot(&self, slot: usize) -> Option<&Transport> {
        self.virtio
            .get(slot.checked_sub(platform::FIRST_VIRTIO_SLOT)?)
    }

    /// The virtio device whose BAR is at `address`, and the offset into it.
    fn virtio_bar(&self, address: u64) -> Option<(&Transport, u64)> {
        let (slot, _, offset) = self.pci.find_bar(address)?;
        Some((self.virtio_in_slot(slot)?, offset))
    }

    /// The offset of `address` in the PCI bus' configuration space.
    fn ecam_offset(address: u64) -> Option<u64> {
        address
            .checked_sub(platform::PCI_ECAM_BASE)
            .filter(|&offset| offset < pci::ECAM_SIZE)
    }

    /// MMIO read of `size` bytes.
    pub(crate) fn mmio_read(&self, address: u64, size: u8) -> u64 {
        if let Some(offset) = Self::ecam_offset(address) {
            self.device_counters.pci.reads.add();
            return self.pci.config_read(offset, size);
        }
        if let Some((transport, offset)) = self.virtio_bar(address) {
            return transport.bar_read(offset, size);
        }
        self.device_counters.unhandled_mmio.reads.add();
        self.report(
            Unhandled::Mmio(address),
            format_args!("MMIO read of {size} bytes at {address:#x}"),
        );
        u64::MAX
    }

    /// MMIO write of `size` bytes.
    pub(crate) fn mmio_write(&self, address: u64, size: u8, value: u64) {
        let send = |address, data| self.signal_msi(address, data);
        if let Some(offset) = Self::ecam_offset(address) {
            self.device_counters.pci.writes.add();
            // The effects hold the configuration space's lock until they are applied.
            if let Some(effects) = self.pci.config_write(offset, size, value)
                && let Some(control) = effects.msix_control
                && let Some(transport) = self.virtio_in_slot(effects.slot)
            {
                transport.set_msix_control(control, send);
            }
            return;
        }
        if let Some((transport, offset)) = self.virtio_bar(address) {
            match transport.bar_write(offset, size, value, send) {
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
        self.device_counters.unhandled_mmio.writes.add();
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
