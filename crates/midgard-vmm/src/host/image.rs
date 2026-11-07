//! Disk images: raw files.
//!
//! I/O is asynchronous: [`Image`]'s operations return futures that move data directly
//! between the image and guest memory. Here, they run pread() and pwrite() on
//! [`blocking`]'s thread pool, and the kernel copies the data from or to guest memory;
//! native asynchronous file I/O (e.g., managarm's) can implement the same interface.

use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use blocking::unblock;
use midgard_hv::GuestMemory;

/// An operation of an [`Image`]. Once it started, dropping the future does not stop it
/// (and the guest memory that it got stays mapped until it is done).
pub(crate) type Io = Pin<Box<dyn Future<Output = io::Result<()>> + Send>>;

/// A buffer in guest memory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Buffer {
    pub(crate) address: u64,
    pub(crate) length: usize,
}

pub(crate) trait Image: Send + Sync {
    /// The size of the guest's disk in bytes.
    fn size(&self) -> u64;
    /// Reads the disk from `offset` on into `buffers` (one after the other).
    fn read(&self, memory: GuestMemory, buffers: Vec<Buffer>, offset: u64) -> Io;
    /// Writes `buffers` (one after the other) to the disk from `offset` on.
    fn write(&self, memory: GuestMemory, buffers: Vec<Buffer>, offset: u64) -> Io;
    /// Makes the writes that completed so far durable.
    fn flush(&self) -> Io;
}

/// Opens a raw image.
pub(crate) fn open(path: &Path, read_only: bool) -> io::Result<Arc<dyn Image>> {
    let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
    lock(&file, read_only)?;
    Ok(Arc::new(Raw::new(file)?))
}

/// Locks an image against other VMs (like QEMU does): exclusively if we write it,
/// shared otherwise. The lock goes away with the file, after its last I/O.
fn lock(file: &File, read_only: bool) -> io::Result<()> {
    let result = if read_only {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match result {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "the image is in use (by another VM?)",
        )),
        // The platform has no file locks.
        Err(TryLockError::Error(error)) if error.kind() == io::ErrorKind::Unsupported => Ok(()),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Checks that `length` bytes at `offset` are inside a disk of `size` bytes.
fn check_range(size: u64, offset: u64, length: u64) -> io::Result<()> {
    if offset.checked_add(length).is_none_or(|end| end > size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "access beyond the end of the disk",
        ));
    }
    Ok(())
}

/// Calls `f` for each buffer with its offset on a disk of `size` bytes (from `offset` on),
/// after checking that all of them fit.
fn for_each_buffer(
    size: u64,
    buffers: &[Buffer],
    mut offset: u64,
    mut f: impl FnMut(Buffer, u64) -> io::Result<()>,
) -> io::Result<()> {
    let length = buffers.iter().map(|buffer| buffer.length as u64).sum();
    check_range(size, offset, length)?;
    for &buffer in buffers {
        f(buffer, offset)?;
        offset += buffer.length as u64;
    }
    Ok(())
}

/// The file offset for pread() and pwrite().
fn file_offset(offset: u64) -> io::Result<libc::off_t> {
    libc::off_t::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput.into())
}

/// Reads `length` bytes at `offset` of `file` into guest memory at `address`.
fn read_into(
    file: &File,
    memory: &GuestMemory,
    address: u64,
    length: usize,
    offset: u64,
) -> io::Result<()> {
    let pointer = memory.pointer(address, length).map_err(io::Error::other)?;
    let mut done = 0;
    while done < length {
        // SAFETY: The buffer is inside guest memory, which `memory` keeps mapped, and the
        // kernel (not Rust) accesses it (see GuestMemory::pointer).
        let count = unsafe {
            libc::pread(
                file.as_raw_fd(),
                pointer.wrapping_add(done).cast(),
                length - done,
                file_offset(offset + done as u64)?,
            )
        };
        match count {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            1.. => done += count as usize,
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

/// Writes `length` bytes from guest memory at `address` to `file` at `offset`.
fn write_from(
    file: &File,
    memory: &GuestMemory,
    address: u64,
    length: usize,
    offset: u64,
) -> io::Result<()> {
    let pointer = memory.pointer(address, length).map_err(io::Error::other)?;
    let mut done = 0;
    while done < length {
        // SAFETY: See read_into().
        let count = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                pointer.wrapping_add(done).cast(),
                length - done,
                file_offset(offset + done as u64)?,
            )
        };
        match count {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            1.. => done += count as usize,
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Raw {
    file: Arc<File>,
    size: u64,
}

impl Raw {
    pub(crate) fn new(file: File) -> io::Result<Self> {
        let size = file.metadata()?.len();
        Ok(Self {
            file: Arc::new(file),
            size,
        })
    }
}

impl Image for Raw {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, memory: GuestMemory, buffers: Vec<Buffer>, offset: u64) -> Io {
        let image = self.clone();
        Box::pin(unblock(move || {
            for_each_buffer(image.size, &buffers, offset, |buffer, offset| {
                read_into(&image.file, &memory, buffer.address, buffer.length, offset)
            })
        }))
    }

    fn write(&self, memory: GuestMemory, buffers: Vec<Buffer>, offset: u64) -> Io {
        let image = self.clone();
        // The file does not grow (see for_each_buffer()).
        Box::pin(unblock(move || {
            for_each_buffer(image.size, &buffers, offset, |buffer, offset| {
                write_from(&image.file, &memory, buffer.address, buffer.length, offset)
            })
        }))
    }

    fn flush(&self) -> Io {
        let file = self.file.clone();
        Box::pin(unblock(move || file.sync_data()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    /// Serializes the tests that open images: a test that runs qemu-img forks, and until
    /// the child execs, it holds the other tests' files (and their locks).
    fn exclusive() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Writes `data` to the disk at `offset` (through guest memory).
    fn write_disk(image: &dyn Image, data: &[u8], offset: u64) -> io::Result<()> {
        let memory = GuestMemory::detached(data.len());
        memory.write(0, data).unwrap();
        let buffer = Buffer {
            address: 0,
            length: data.len(),
        };
        async_io::block_on(image.write(memory, vec![buffer], offset))
    }

    /// Reads from the disk at `offset` into `buffer` (through guest memory).
    fn read_disk(image: &dyn Image, buffer: &mut [u8], offset: u64) -> io::Result<()> {
        let memory = GuestMemory::detached(buffer.len());
        let buffers = vec![Buffer {
            address: 0,
            length: buffer.len(),
        }];
        async_io::block_on(image.read(memory.clone(), buffers, offset))?;
        memory.read(0, buffer).unwrap();
        Ok(())
    }

    #[test]
    fn raw_round_trip() {
        let _exclusive = exclusive();
        let dir = std::env::temp_dir().join(format!("midgard-raw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.img");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        let image = open(&path, false).unwrap();
        assert_eq!(image.size(), 4096);
        write_disk(&*image, b"hello", 4091).unwrap();
        assert!(write_disk(&*image, b"x", 4096).is_err());
        async_io::block_on(image.flush()).unwrap();
        drop(image);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096);

        let image = open(&path, true).unwrap();
        let mut buffer = [0u8; 5];
        read_disk(&*image, &mut buffer, 4091).unwrap();
        assert_eq!(&buffer, b"hello");
        assert!(read_disk(&*image, &mut buffer, 4092).is_err());
        assert!(write_disk(&*image, b"x", 0).is_err());
        // Several buffers continue on the disk where the previous one ended.
        let memory = GuestMemory::detached(64);
        let buffers = vec![
            Buffer {
                address: 32,
                length: 2,
            },
            Buffer {
                address: 0,
                length: 3,
            },
        ];
        async_io::block_on(image.read(memory.clone(), buffers, 4091)).unwrap();
        let mut bytes = [0u8; 34];
        memory.read(0, &mut bytes).unwrap();
        assert_eq!((&bytes[32..], &bytes[..3]), (&b"he"[..], &b"llo"[..]));

        // Other VMs may read the image, but not write it, while it is open.
        assert!(open(&path, true).is_ok());
        assert!(open(&path, false).is_err());
        drop(image);
        assert!(open(&path, false).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
