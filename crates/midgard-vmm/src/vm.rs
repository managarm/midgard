//! State that is shared by all vCPUs: guest memory and the devices. The interrupt
//! controllers (local APICs and the I/O APIC) are the hypervisor's.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use midgard_hv::{GuestMemory, Hypervisor, Kicker};

use crate::devices::pm1::{self, Pm1};
use crate::devices::serial::{self, Serial};
use crate::devices::{read_bytes, write_bytes};
use crate::log;
use crate::logging::ReportOnce;
use crate::platform;

/// Why the VM stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The guest requested a reset (keyboard controller, port 0xCF9 or a triple fault).
    Reset,
    /// The guest entered S5 (ACPI poweroff).
    PowerOff,
    /// We stopped the VM (see [`Vm::stop`]).
    Requested,
}

/// Problems that are reported once (see [`ReportOnce`]).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Unhandled {
    /// Setting an interrupt line failed.
    IrqLine(usize),
    Port(u16),
    Mmio(u64),
}

pub struct Vm {
    /// Declared (and thus dropped) before `memory`, whose mapping the VM in the
    /// hypervisor references.
    hypervisor: Hypervisor,
    pub memory: GuestMemory,
    pub num_cpus: u32,
    /// The TSC frequency in Hz.
    pub tsc_hz: u64,
    serial: Mutex<Serial>,
    /// The level of the serial port's interrupt line.
    serial_irq: AtomicBool,
    pm1: Mutex<Pm1>,
    /// The vCPUs' kickers (see [`Vm::stop`]).
    kickers: Mutex<Vec<Kicker>>,
    /// [`Vm::stop`] was called.
    stopping: AtomicBool,
    reported: ReportOnce<Unhandled>,
}

impl Vm {
    /// The serial port's output goes to `console`.
    pub fn new(
        memory: GuestMemory,
        num_cpus: u32,
        tsc_hz: u64,
        hypervisor: Hypervisor,
        console: Box<dyn Write + Send>,
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
            reported: ReportOnce::new(),
        }
    }

    /// The VM in the hypervisor (e.g., for creating vCPUs).
    pub fn hypervisor(&self) -> &Hypervisor {
        &self.hypervisor
    }

    /// Lets [`Vm::stop`] kick a vCPU.
    pub(crate) fn add_kicker(&self, kicker: Kicker) {
        self.kickers.lock().unwrap().push(kicker);
    }

    /// Makes all vCPUs stop with [`Stop::Requested`] (from any thread).
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        for kicker in self.kickers.lock().unwrap().iter() {
            kicker.kick();
        }
    }

    /// Whether [`Vm::stop`] was called.
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

    pub fn flush_console(&self) {
        self.serial.lock().unwrap().flush();
    }

    fn report(&self, key: Unhandled, what: std::fmt::Arguments) {
        if self.reported.first(key) {
            log!("unhandled {what} (further accesses are not reported)");
        }
    }

    /// Port input of `size` bytes.
    pub fn pio_in(&self, port: u16, size: u32) -> u32 {
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
    pub fn pio_out(&self, port: u16, size: u32, value: u32) -> Option<Stop> {
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

    /// MMIO read of `size` bytes.
    pub fn mmio_read(&self, address: u64, size: u8) -> u64 {
        self.report(
            Unhandled::Mmio(address),
            format_args!("MMIO read of {size} bytes at {address:#x}"),
        );
        u64::MAX
    }

    /// MMIO write of `size` bytes.
    pub fn mmio_write(&self, address: u64, size: u8, _value: u64) {
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
