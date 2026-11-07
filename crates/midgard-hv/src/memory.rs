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

pub struct GuestMemory {
    base: *mut u8,
    size: u64,
    /// Whatever keeps the memory mapped (dropped last).
    _owner: Owner,
}

enum Owner {
    /// The backend's mapping of the VM's RAM.
    Backend(#[allow(dead_code)] Arc<imp::Mapping>),
    /// Memory that no VM uses (see [`GuestMemory::detached`]).
    Heap(#[allow(dead_code)] Box<[u8]>),
}

// SAFETY: Guest memory is shared with the guest (which runs concurrently anyway);
// we only access it through raw pointers.
unsafe impl Send for GuestMemory {}
// SAFETY: All accessors take `&self` and go through raw pointers, so concurrent calls
// are no different from the guest accessing the memory concurrently.
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// # Safety
    /// `base` must point to `size` bytes of memory that stay mapped while `mapping` lives.
    pub(crate) unsafe fn new(base: *mut u8, size: u64, mapping: Arc<imp::Mapping>) -> Self {
        Self {
            base,
            size,
            _owner: Owner::Backend(mapping),
        }
    }

    /// Zeroed memory of `size` bytes that no VM uses (e.g., for testing devices).
    pub fn detached(size: usize) -> Self {
        let mut heap = vec![0u8; size].into_boxed_slice();
        Self {
            base: heap.as_mut_ptr(),
            size: size as u64,
            _owner: Owner::Heap(heap),
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns a pointer to `length` bytes of guest memory at `address`.
    pub fn pointer(&self, address: u64, length: usize) -> Result<*mut u8, MemoryError> {
        if address
            .checked_add(length as u64)
            .is_none_or(|end| end > self.size)
        {
            return Err(MemoryError::OutOfRange { address, length });
        }
        // SAFETY: The range is inside the mapping.
        Ok(unsafe { self.base.add(address as usize) })
    }

    pub fn write(&self, address: u64, data: &[u8]) -> Result<(), MemoryError> {
        let pointer = self.pointer(address, data.len())?;
        // SAFETY: See pointer(). The guest may access the memory concurrently;
        // like real DMA, that is the guest's problem.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), pointer, data.len()) };
        Ok(())
    }

    pub fn read(&self, address: u64, buffer: &mut [u8]) -> Result<(), MemoryError> {
        let pointer = self.pointer(address, buffer.len())?;
        // SAFETY: See write().
        unsafe { std::ptr::copy_nonoverlapping(pointer, buffer.as_mut_ptr(), buffer.len()) };
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

    /// Reads a naturally aligned value that the guest may modify concurrently.
    pub fn read_volatile<T: FromBytes>(&self, address: u64) -> Result<T, MemoryError> {
        let pointer = self.aligned_pointer::<T>(address)?;
        // SAFETY: The value is inside guest memory and aligned, and any bytes are a valid
        // T (FromBytes).
        Ok(unsafe { std::ptr::read_volatile(pointer) })
    }

    /// Writes a naturally aligned value that the guest may read concurrently.
    pub fn write_volatile<T: IntoBytes + Immutable>(
        &self,
        address: u64,
        value: T,
    ) -> Result<(), MemoryError> {
        let pointer = self.aligned_pointer::<T>(address)?;
        // SAFETY: See read_volatile().
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
