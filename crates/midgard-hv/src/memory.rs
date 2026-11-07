//! Guest RAM: one contiguous region at guest-physical address zero that is also mapped
//! into our own address space. The backend allocates it (see [`crate::create`]).

use std::sync::Arc;

use thiserror::Error;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::imp;

/// Why an access to guest memory failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MemoryError {
    #[error("{length} bytes at {address:#x} are outside of guest RAM")]
    OutOfRange { address: u64, length: usize },
    #[error("{address:#x} is not aligned to {align} bytes")]
    Misaligned { address: u64, align: usize },
}

/// Guest RAM, as we access it.
///
/// The guest (and the kernel) may access guest memory at any time, which the Rust memory
/// model does not know about. Hence, guest memory is never part of a Rust allocation: we
/// never create references to it and only access it through volatile accesses, which Rust
/// permits for memory outside of its allocations (see [`std::ptr::read_volatile`]).
pub struct GuestMemory {
    base: *mut u8,
    size: u64,
    /// Keeps the memory mapped.
    _mapping: Arc<imp::Mapping>,
}

// SAFETY: Guest memory is outside of Rust allocations and only accessed through volatile
// accesses (see above), which may happen on any thread.
unsafe impl Send for GuestMemory {}
// SAFETY: Like for the guest, concurrent volatile accesses to memory outside of Rust
// allocations are not data races.
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// # Safety
    /// `base` must point to `size` bytes of memory that are outside of Rust allocations
    /// and stay mapped while `mapping` lives.
    pub(crate) unsafe fn new(base: *mut u8, size: u64, mapping: Arc<imp::Mapping>) -> Self {
        Self {
            base,
            size,
            _mapping: mapping,
        }
    }

    /// Zeroed memory of `size` bytes that no VM uses (e.g., for testing devices).
    pub fn detached(size: usize) -> Self {
        // The backend cannot map zero bytes.
        let mapping = imp::Mapping::new(size.max(1)).expect("failed to allocate memory");
        // SAFETY: The mapping is new and stays mapped while it lives.
        unsafe { Self::new(mapping.base(), size as u64, Arc::new(mapping)) }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns a pointer to `length` bytes of guest memory at `address`. Accesses through
    /// it must be volatile (see [`GuestMemory`]) or happen outside of Rust (e.g., the
    /// kernel's, in a read() system call).
    pub fn pointer(&self, address: u64, length: usize) -> Result<*mut u8, MemoryError> {
        if address
            .checked_add(length as u64)
            .is_none_or(|end| end > self.size)
        {
            return Err(MemoryError::OutOfRange { address, length });
        }
        // Guest memory is not a Rust allocation, so we cannot use add().
        Ok(self.base.wrapping_add(address as usize))
    }

    /// Copies `data` to guest memory at `address`. The guest may access the memory
    /// concurrently; like real DMA, that is the guest's problem.
    pub fn write(&self, address: u64, data: &[u8]) -> Result<(), MemoryError> {
        let pointer = self.pointer(address, data.len())?;
        let mut offset = 0;
        while offset < data.len() {
            let target = pointer.wrapping_add(offset);
            if let Some(chunk) = data[offset..].first_chunk::<8>()
                && target.cast::<u64>().is_aligned()
            {
                // SAFETY: The chunk is aligned and inside guest memory (see pointer()).
                unsafe {
                    target
                        .cast::<u64>()
                        .write_volatile(u64::from_ne_bytes(*chunk))
                };
                offset += 8;
            } else {
                // SAFETY: The byte is inside guest memory.
                unsafe { target.write_volatile(data[offset]) };
                offset += 1;
            }
        }
        Ok(())
    }

    /// Copies guest memory at `address` to `buffer`. See [`GuestMemory::write`].
    pub fn read(&self, address: u64, buffer: &mut [u8]) -> Result<(), MemoryError> {
        let pointer = self.pointer(address, buffer.len())?;
        let mut offset = 0;
        while offset < buffer.len() {
            let source = pointer.wrapping_add(offset);
            if let Some(chunk) = buffer[offset..].first_chunk_mut::<8>()
                && source.cast::<u64>().is_aligned()
            {
                // SAFETY: The chunk is aligned and inside guest memory (see pointer()).
                *chunk = unsafe { source.cast::<u64>().read_volatile() }.to_ne_bytes();
                offset += 8;
            } else {
                // SAFETY: The byte is inside guest memory.
                buffer[offset] = unsafe { source.read_volatile() };
                offset += 1;
            }
        }
        Ok(())
    }

    pub fn read_u64(&self, address: u64) -> Result<u64, MemoryError> {
        let mut bytes = [0u8; 8];
        self.read(address, &mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// Returns a pointer to a naturally aligned `T` at `address`.
    fn aligned_pointer<T>(&self, address: u64) -> Result<*mut T, MemoryError> {
        let align = align_of::<T>();
        if !address.is_multiple_of(align as u64) {
            return Err(MemoryError::Misaligned { address, align });
        }
        Ok(self.pointer(address, size_of::<T>())?.cast::<T>())
    }

    /// Reads a naturally aligned value that the guest may modify concurrently. Unlike
    /// [`GuestMemory::read`], this reads integers with a single access (that does not tear).
    pub fn read_volatile<T: FromBytes>(&self, address: u64) -> Result<T, MemoryError> {
        let pointer = self.aligned_pointer::<T>(address)?;
        // SAFETY: The value is inside guest memory and aligned, and any bytes are a valid
        // T (FromBytes).
        Ok(unsafe { std::ptr::read_volatile(pointer) })
    }

    /// Writes a naturally aligned value that the guest may read concurrently. See
    /// [`GuestMemory::read_volatile`].
    pub fn write_volatile<T: IntoBytes + Immutable>(
        &self,
        address: u64,
        value: T,
    ) -> Result<(), MemoryError> {
        let pointer = self.aligned_pointer::<T>(address)?;
        // SAFETY: The value is inside guest memory and aligned.
        unsafe { std::ptr::write_volatile(pointer, value) };
        Ok(())
    }

    /// Reads a plain-data object (e.g., a structure that the guest set up).
    pub fn read_obj<T: FromBytes + IntoBytes>(&self, address: u64) -> Result<T, MemoryError> {
        let mut value = T::new_zeroed();
        self.read(address, value.as_mut_bytes())?;
        Ok(value)
    }

    pub fn write_obj<T: IntoBytes + Immutable + ?Sized>(
        &self,
        address: u64,
        value: &T,
    ) -> Result<(), MemoryError> {
        self.write(address, value.as_bytes())
    }

    pub fn write_u64(&self, address: u64, value: u64) -> Result<(), MemoryError> {
        self.write(address, &value.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unaligned_copies() {
        let memory = GuestMemory::detached(64);
        let data: Vec<u8> = (1..=21).collect();
        memory.write(3, &data).unwrap();
        let mut buffer = [0u8; 25];
        memory.read(1, &mut buffer).unwrap();
        assert_eq!(buffer[..2], [0, 0]);
        assert_eq!(buffer[2..23], data[..]);
        assert_eq!(buffer[23..], [0, 0]);
    }

    #[test]
    fn out_of_range() {
        let memory = GuestMemory::detached(16);
        assert_eq!(
            memory.write(10, &[0; 8]),
            Err(MemoryError::OutOfRange {
                address: 10,
                length: 8
            })
        );
        assert_eq!(memory.read_u64(8), Ok(0));
    }
}
