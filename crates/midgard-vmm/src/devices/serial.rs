//! A 16550A UART (COM1, IRQ 4) whose output goes to the VM's console.

use std::collections::VecDeque;

use crate::host::console::ConsoleOutput;

pub(crate) const NUM_PORTS: u16 = 8;

const LCR_DLAB: u8 = 1 << 7;
const MCR_LOOP: u8 = 1 << 4;
const FCR_ENABLE_FIFO: u8 = 1 << 0;
const FCR_CLEAR_RX: u8 = 1 << 1;
const IER_RDI: u8 = 1 << 0;
const IER_THRI: u8 = 1 << 1;
const LSR_DATA_READY: u8 = 1 << 0;
/// Transmitter holding register empty, transmitter empty.
const LSR_IDLE: u8 = 0x60;
const IIR_NO_INTERRUPT: u8 = 0x01;
const IIR_THR_EMPTY: u8 = 0x02;
const IIR_RX_DATA: u8 = 0x04;
const IIR_FIFO_ENABLED: u8 = 0xC0;
/// DCD, DSR, CTS.
const MSR_CONNECTED: u8 = 0xB0;

pub(crate) struct Serial {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    fifo: bool,
    /// A "transmitter holding register empty" interrupt is pending.
    thr_pending: bool,
    input: VecDeque<u8>,
    output: ConsoleOutput,
}

impl Serial {
    pub(crate) fn new(output: ConsoleOutput) -> Self {
        Self {
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0,
            dlm: 0,
            fifo: false,
            thr_pending: false,
            input: VecDeque::new(),
            output,
        }
    }

    /// The level of the interrupt line.
    pub(crate) fn irq_level(&self) -> bool {
        (self.ier & IER_RDI != 0 && !self.input.is_empty())
            || (self.ier & IER_THRI != 0 && self.thr_pending)
    }

    pub(crate) fn read(&mut self, offset: u16) -> u8 {
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            0 if dlab => self.dll,
            0 => self.input.pop_front().unwrap_or(0),
            1 if dlab => self.dlm,
            1 => self.ier,
            2 => {
                let fifo = if self.fifo { IIR_FIFO_ENABLED } else { 0 };
                if self.ier & IER_RDI != 0 && !self.input.is_empty() {
                    IIR_RX_DATA | fifo
                } else if self.ier & IER_THRI != 0 && self.thr_pending {
                    // Reading the IIR acknowledges a THR empty interrupt.
                    self.thr_pending = false;
                    IIR_THR_EMPTY | fifo
                } else {
                    IIR_NO_INTERRUPT | fifo
                }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => {
                LSR_IDLE
                    | if self.input.is_empty() {
                        0
                    } else {
                        LSR_DATA_READY
                    }
            }
            6 if self.mcr & MCR_LOOP != 0 => {
                // In loopback mode, the modem control outputs drive the inputs:
                // RTS -> CTS, DTR -> DSR, OUT1 -> RI, OUT2 -> DCD.
                let m = self.mcr;
                ((m & 0x2) << 3) | ((m & 0x1) << 5) | ((m & 0x4) << 4) | ((m & 0x8) << 4)
            }
            6 => MSR_CONNECTED,
            7 => self.scr,
            _ => 0xFF,
        }
    }

    pub(crate) fn write(&mut self, offset: u16, value: u8) {
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            0 if dlab => self.dll = value,
            0 => {
                if self.mcr & MCR_LOOP == 0 {
                    self.output.write(&[value]);
                }
                // The character is sent immediately; the THR is empty again.
                self.thr_pending = true;
            }
            1 if dlab => self.dlm = value,
            1 => {
                let enabled = value & IER_THRI != 0 && self.ier & IER_THRI == 0;
                self.ier = value & 0x0F;
                if enabled {
                    // Enabling the interrupt while the THR is empty raises it.
                    self.thr_pending = true;
                } else if self.ier & IER_THRI == 0 {
                    self.thr_pending = false;
                }
            }
            2 => {
                self.fifo = value & FCR_ENABLE_FIFO != 0;
                if value & FCR_CLEAR_RX != 0 {
                    self.input.clear();
                }
            }
            3 => self.lcr = value,
            4 => self.mcr = value & 0x1F,
            7 => self.scr = value,
            _ => {}
        }
    }
}
