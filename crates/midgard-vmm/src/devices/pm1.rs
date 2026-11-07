//! The ACPI PM1a event and control blocks. We never raise events; the guest uses the
//! control block to power off.

/// The length of the PM1a event block (status and enable registers, 2 bytes each).
pub(crate) const EVT_LEN: u16 = 4;
/// The length of the PM1a control block.
pub(crate) const CNT_LEN: u16 = 2;
/// PM1 control: the OS uses ACPI (we are always in ACPI mode).
const CNT_SCI_EN: u16 = 1 << 0;
/// PM1 control: enter the sleep state in SLP_TYP.
const CNT_SLP_EN: u16 = 1 << 13;
const CNT_SLP_TYP_SHIFT: u16 = 10;
/// The SLP_TYP value of S5 (soft off), as \_S5 declares it.
pub(crate) const S5_SLP_TYP: u16 = 5;

#[derive(Default)]
pub(crate) struct Pm1 {
    /// The enable register. ACPICA checks that the bits that it enables read back.
    enable: u16,
}

impl Pm1 {
    /// Reads byte `offset` of the event block.
    pub(crate) fn read_event(&self, offset: u16) -> u8 {
        // The status register (no events are pending), then the enable register.
        let registers = (self.enable as u32) << 16;
        (registers >> (8 * offset)) as u8
    }

    /// Writes byte `offset` of the event block.
    pub(crate) fn write_event(&mut self, offset: u16, value: u8) {
        // Writes to the status register (bytes 0 and 1) clear bits that are never set.
        if let Some(byte) = offset.checked_sub(2) {
            let shift = 8 * byte;
            self.enable = self.enable & !(0xFF << shift) | (value as u16) << shift;
        }
    }

    /// Reads byte `offset` of the control block.
    pub(crate) fn read_control(&self, offset: u16) -> u8 {
        (CNT_SCI_EN >> (8 * offset)) as u8
    }

    /// Writes byte `offset` of the control block; returns whether the write powers the
    /// machine off.
    pub(crate) fn write_control(&mut self, offset: u16, value: u8) -> bool {
        // SCI_EN stays set; other sleep states are not declared. SLP_EN and SLP_TYP are
        // in the high byte.
        let value = (value as u16) << (8 * offset);
        offset == 1 && value & CNT_SLP_EN != 0 && (value >> CNT_SLP_TYP_SHIFT) & 0x7 == S5_SLP_TYP
    }
}
