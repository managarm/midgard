//! The guest's legacy PC devices on I/O ports: the 16550A UART and the ACPI PM1 blocks.
//! Their registers are byte-wide; [`read_bytes`] and [`write_bytes`] split wider
//! accesses.

pub(crate) mod pm1;
pub(crate) mod serial;

/// Reads `size` bytes at `offset` of a device with `len` byte-wide registers, one
/// register per byte (little endian). Bytes past the device read as 0xFF.
pub(crate) fn read_bytes(len: u16, offset: u16, size: u32, mut read: impl FnMut(u16) -> u8) -> u32 {
    let mut value = 0u32;
    for i in 0..size {
        let offset = offset + i as u16;
        let byte = if offset < len { read(offset) } else { 0xFF };
        value |= (byte as u32) << (8 * i);
    }
    value
}

/// Writes `size` bytes at `offset` of a device with `len` byte-wide registers, one
/// register per byte (little endian). Bytes past the device are dropped.
pub(crate) fn write_bytes(
    len: u16,
    offset: u16,
    size: u32,
    value: u32,
    mut write: impl FnMut(u16, u8),
) {
    for i in 0..size {
        let offset = offset + i as u16;
        if offset < len {
            write(offset, (value >> (8 * i)) as u8);
        }
    }
}
