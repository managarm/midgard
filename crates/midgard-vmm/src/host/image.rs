//! Disk images: raw files and qcow2.
//!
//! I/O is asynchronous: [`Image`]'s operations return futures that move data directly
//! between the image and guest memory. Here, they run pread() and pwrite() on
//! [`blocking`]'s thread pool, and the kernel copies the data from or to guest memory;
//! native asynchronous file I/O (e.g., managarm's) can implement the same interface.
//!
//! qcow2 images are sparse: the guest's disk is divided into clusters, and only clusters
//! that were written take up space in the file. Two levels of tables (L1, L2) map guest
//! clusters to file offsets; refcounts track which clusters of the file are in use.
//! We support what `qemu-img create` / `convert` produce: version 2 or 3, 16-bit
//! refcounts, no backing files, snapshots, encryption or compressed clusters.
//! New clusters are appended to the file. Images can grow (see [`grow`]), but not
//! shrink.

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use blocking::unblock;
use midgard_hv::GuestMemory;
use zerocopy::big_endian::{U32, U64};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

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

/// Opens a raw or qcow2 image (detected by its magic).
pub(crate) fn open(path: &Path, read_only: bool) -> io::Result<Arc<dyn Image>> {
    let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
    lock(&file, read_only)?;
    if is_qcow2(&file) {
        Ok(Arc::new(Qcow2::open(file)?))
    } else {
        Ok(Arc::new(Raw::new(file)?))
    }
}

fn is_qcow2(file: &File) -> bool {
    let mut magic = [0u8; 4];
    file.read_exact_at(&mut magic, 0).is_ok() && magic == QCOW2_MAGIC
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

/// The size of the guest's disk in an image (which a VM may be using).
pub fn size(path: &Path) -> io::Result<u64> {
    let file = File::open(path)?;
    if is_qcow2(&file) {
        Ok(Qcow2::open(file)?.size)
    } else {
        Ok(Raw::new(file)?.size)
    }
}

/// Grows the guest's disk in an image to `size` bytes (the new space reads as zeros).
/// Fails if a VM is using the image.
pub fn grow(path: &Path, size: u64) -> io::Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    lock(&file, false)?;
    if is_qcow2(&file) {
        Qcow2::open(file)?.grow(size)
    } else {
        Raw::new(file)?.grow(size)
    }
}

fn cannot_shrink(from: u64, to: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("the disk cannot shrink (from {from} to {to} bytes)"),
    )
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

/// Fills `length` bytes of guest memory at `address` with zeros.
fn zero(memory: &GuestMemory, mut address: u64, mut length: usize) -> io::Result<()> {
    const ZEROS: [u8; 4096] = [0; 4096];
    while length > 0 {
        let count = length.min(ZEROS.len());
        memory
            .write(address, &ZEROS[..count])
            .map_err(io::Error::other)?;
        address += count as u64;
        length -= count;
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

    fn grow(&mut self, size: u64) -> io::Result<()> {
        if size < self.size {
            return Err(cannot_shrink(self.size, size));
        }
        self.file.set_len(size)?;
        self.size = size;
        self.file.sync_all()
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

const QCOW2_MAGIC: [u8; 4] = *b"QFI\xfb";
/// Bits 9-55 of L1 and L2 entries and of refcount table entries.
const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
/// The cluster's refcount is 1 (it may be written in place).
const COPIED: u64 = 1 << 63;
const COMPRESSED: u64 = 1 << 62;
/// (Version 3) The cluster reads as zeros.
const ZERO: u64 = 1;

fn unsupported(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, format!("qcow2: {what}"))
}

fn invalid(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("qcow2: {what}"))
}

/// A qcow2 image. Reads and writes of clusters that exist run concurrently; changes of
/// the tables (when writes allocate clusters) are serialized.
#[derive(Clone)]
pub(crate) struct Qcow2 {
    file: Arc<File>,
    size: u64,
    cluster_bits: u32,
    tables: Arc<Mutex<Tables>>,
}

/// The metadata of a qcow2 image.
struct Tables {
    file: Arc<File>,
    version: u32,
    /// (Version 3) Features that become invalid when an unaware writer modifies the
    /// image (e.g., dirty bitmaps); they are cleared before the first write.
    autoclear: u64,
    cluster_bits: u32,
    l1_offset: u64,
    l1: Vec<u64>,
    refcount_table_offset: u64,
    refcount_table: Vec<u64>,
    /// L2 tables and refcount blocks by file offset (loaded on demand).
    l2_tables: HashMap<u64, Vec<u64>>,
    refcount_blocks: HashMap<u64, Vec<u16>>,
    /// Where the next cluster is allocated (the end of the file).
    end: u64,
}

/// The qcow2 header (big-endian).
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Header {
    magic: [u8; 4],
    version: U32,
    backing_file_offset: U64,
    backing_file_size: U32,
    cluster_bits: U32,
    size: U64,
    crypt_method: U32,
    l1_size: U32,
    l1_table_offset: U64,
    refcount_table_offset: U64,
    refcount_table_clusters: U32,
    nb_snapshots: U32,
    snapshots_offset: U64,
    // Version 3.
    incompatible_features: U64,
    compatible_features: U64,
    autoclear_features: U64,
    refcount_order: U32,
    header_length: U32,
}

/// The length of a version 2 header (without the version 3 fields).
const V2_HEADER_SIZE: usize = std::mem::offset_of!(Header, incompatible_features);

const _: () = assert!(V2_HEADER_SIZE == 72 && size_of::<Header>() == 104);

impl Qcow2 {
    pub(crate) fn open(file: File) -> io::Result<Self> {
        let mut header = Header::new_zeroed();
        file.read_exact_at(&mut header.as_mut_bytes()[..V2_HEADER_SIZE], 0)?;
        let version = header.version.get();
        if version != 2 && version != 3 {
            return Err(unsupported(format!("version {version}")));
        }
        if version == 3 {
            file.read_exact_at(
                &mut header.as_mut_bytes()[V2_HEADER_SIZE..],
                V2_HEADER_SIZE as u64,
            )?;
            let incompatible = header.incompatible_features.get();
            if incompatible != 0 {
                // E.g., dirty (lazy refcounts) or compression types; `qemu-img check -r all`
                // or `qemu-img convert` produce an image without them.
                return Err(unsupported(format!(
                    "incompatible features {incompatible:#x}"
                )));
            }
            let refcount_order = header.refcount_order.get();
            if refcount_order != 4 {
                return Err(unsupported(format!(
                    "{}-bit refcounts",
                    1u64 << refcount_order
                )));
            }
        }
        if header.backing_file_offset.get() != 0 {
            return Err(unsupported("backing files"));
        }
        if header.crypt_method.get() != 0 {
            return Err(unsupported("encryption"));
        }
        if header.nb_snapshots.get() != 0 {
            return Err(unsupported("snapshots"));
        }
        let cluster_bits = header.cluster_bits.get();
        if !(9..=21).contains(&cluster_bits) {
            return Err(invalid(format!("cluster_bits {cluster_bits}")));
        }
        let cluster_size = 1u64 << cluster_bits;
        let size = header.size.get();
        let l1_size = header.l1_size.get() as u64;
        let l1_offset = header.l1_table_offset.get();
        let refcount_table_offset = header.refcount_table_offset.get();
        let refcount_table_clusters = header.refcount_table_clusters.get() as u64;

        let l2_coverage = cluster_size * (cluster_size / 8);
        if l1_size < size.div_ceil(l2_coverage) {
            return Err(invalid("the L1 table is too small"));
        }
        if l1_size > 1 << 24 || refcount_table_clusters > 1 << 16 {
            return Err(invalid("the tables are too large"));
        }
        // The tables are in the file (which also bounds what we allocate for them).
        let file_size = file.metadata()?.len();
        let refcount_table_size = refcount_table_clusters * cluster_size;
        for (offset, length) in [
            (l1_offset, l1_size * 8),
            (refcount_table_offset, refcount_table_size),
        ] {
            if offset.checked_add(length).is_none_or(|end| end > file_size) {
                return Err(invalid("a table is outside of the file"));
            }
        }
        let l1 = read_table(&file, l1_offset, l1_size as usize)?;
        let refcount_table = read_table(
            &file,
            refcount_table_offset,
            (refcount_table_size / 8) as usize,
        )?;
        let end = file_size.next_multiple_of(cluster_size);
        let file = Arc::new(file);
        let tables = Tables {
            file: file.clone(),
            version,
            autoclear: header.autoclear_features.get(),
            cluster_bits,
            l1_offset,
            l1,
            refcount_table_offset,
            refcount_table,
            l2_tables: HashMap::new(),
            refcount_blocks: HashMap::new(),
            end,
        };
        Ok(Self {
            file,
            size,
            cluster_bits,
            tables: Arc::new(Mutex::new(tables)),
        })
    }

    /// Grows the guest's disk (see [`Tables::grow`]).
    fn grow(&mut self, size: u64) -> io::Result<()> {
        self.tables.lock().unwrap().grow(self.size, size)?;
        self.size = size;
        Ok(())
    }

    fn read_blocking(
        &self,
        memory: &GuestMemory,
        mut address: u64,
        length: usize,
        mut offset: u64,
    ) -> io::Result<()> {
        check_range(self.size, offset, length as u64)?;
        memory
            .check_range(address, length)
            .map_err(io::Error::other)?;
        let cluster_size = 1u64 << self.cluster_bits;
        let end = offset + length as u64;
        while offset < end {
            let in_cluster = offset & (cluster_size - 1);
            let count = (end - offset).min(cluster_size - in_cluster) as usize;
            let host = {
                let mut tables = self.tables.lock().unwrap();
                let entry = tables.l2_entry(offset >> self.cluster_bits)?;
                tables.data_offset(entry)?
            };
            match host {
                Some(host) => read_into(&self.file, memory, address, count, host + in_cluster)?,
                None => zero(memory, address, count)?,
            }
            address += count as u64;
            offset += count as u64;
        }
        Ok(())
    }

    fn write_blocking(
        &self,
        memory: &GuestMemory,
        mut address: u64,
        length: usize,
        mut offset: u64,
    ) -> io::Result<()> {
        check_range(self.size, offset, length as u64)?;
        memory
            .check_range(address, length)
            .map_err(io::Error::other)?;
        let cluster_size = 1u64 << self.cluster_bits;
        let end = offset + length as u64;
        while offset < end {
            let in_cluster = offset & (cluster_size - 1);
            let count = (end - offset).min(cluster_size - in_cluster) as usize;
            let guest_cluster = offset >> self.cluster_bits;
            let mut tables = self.tables.lock().unwrap();
            tables.clear_autoclear()?;
            let entry = tables.l2_entry(guest_cluster)?;
            match tables.data_offset(entry)? {
                // Without snapshots, every cluster has a refcount of 1.
                Some(host) => {
                    drop(tables);
                    write_from(&self.file, memory, address, count, host + in_cluster)?;
                }
                // The cluster changes while the tables are locked (such that nobody sees
                // it half-written).
                None => {
                    let host = if entry & OFFSET_MASK != 0 {
                        // A zero cluster that has a host cluster (e.g., preallocated).
                        let host = entry & OFFSET_MASK;
                        self.file
                            .write_all_at(&vec![0u8; cluster_size as usize], host)?;
                        host
                    } else {
                        // The rest of a new cluster reads as zeros (it is past the old end).
                        tables.allocate()?
                    };
                    write_from(&self.file, memory, address, count, host + in_cluster)?;
                    tables.set_l2_entry(guest_cluster, host | COPIED)?;
                }
            }
            address += count as u64;
            offset += count as u64;
        }
        Ok(())
    }
}

impl Tables {
    fn cluster_size(&self) -> u64 {
        1 << self.cluster_bits
    }

    fn l2_entries(&self) -> u64 {
        self.cluster_size() / 8
    }

    fn l2_table(&mut self, offset: u64) -> io::Result<&mut Vec<u64>> {
        if !self.l2_tables.contains_key(&offset) {
            let table = read_table(&self.file, offset, self.l2_entries() as usize)?;
            self.l2_tables.insert(offset, table);
        }
        Ok(self.l2_tables.get_mut(&offset).unwrap())
    }

    /// The L2 entry of a guest cluster (0 if it has no L2 table).
    fn l2_entry(&mut self, guest_cluster: u64) -> io::Result<u64> {
        let l1_index = (guest_cluster / self.l2_entries()) as usize;
        let l2_offset = self.l1[l1_index] & OFFSET_MASK;
        if l2_offset == 0 {
            return Ok(0);
        }
        let index = (guest_cluster % self.l2_entries()) as usize;
        Ok(self.l2_table(l2_offset)?[index])
    }

    /// Where the data of an L2 entry is (None: it reads as zeros).
    fn data_offset(&self, entry: u64) -> io::Result<Option<u64>> {
        if entry & COMPRESSED != 0 {
            return Err(unsupported("compressed clusters"));
        }
        let offset = entry & OFFSET_MASK;
        if offset == 0 || (self.version >= 3 && entry & ZERO != 0) {
            return Ok(None);
        }
        Ok(Some(offset))
    }

    /// Allocates a zeroed cluster at the end of the file.
    fn allocate(&mut self) -> io::Result<u64> {
        let offset = self.end;
        self.end += self.cluster_size();
        self.file.set_len(self.end)?;
        self.set_refcount(offset, 1)?;
        Ok(offset)
    }

    fn set_refcount(&mut self, offset: u64, value: u16) -> io::Result<()> {
        let per_block = self.cluster_size() / 2;
        let cluster = offset >> self.cluster_bits;
        let table_index = (cluster / per_block) as usize;
        if table_index >= self.refcount_table.len() {
            return Err(unsupported("the image outgrew its refcount table"));
        }
        let mut block_offset = self.refcount_table[table_index] & OFFSET_MASK;
        if block_offset == 0 {
            // A new refcount block, which counts itself if it is in its own range.
            block_offset = self.end;
            self.end += self.cluster_size();
            self.file.set_len(self.end)?;
            self.refcount_blocks
                .insert(block_offset, vec![0; per_block as usize]);
            self.refcount_table[table_index] = block_offset;
            self.file.write_all_at(
                &block_offset.to_be_bytes(),
                self.refcount_table_offset + table_index as u64 * 8,
            )?;
            self.set_refcount(block_offset, 1)?;
        }
        if !self.refcount_blocks.contains_key(&block_offset) {
            let mut bytes = vec![0u8; self.cluster_size() as usize];
            self.file.read_exact_at(&mut bytes, block_offset)?;
            let (entries, _) = bytes.as_chunks::<2>();
            let block = entries.iter().map(|&e| u16::from_be_bytes(e)).collect();
            self.refcount_blocks.insert(block_offset, block);
        }
        let index = cluster % per_block;
        self.refcount_blocks.get_mut(&block_offset).unwrap()[index as usize] = value;
        self.file
            .write_all_at(&value.to_be_bytes(), block_offset + index * 2)
    }

    /// Clears the autoclear features (before the first write).
    fn clear_autoclear(&mut self) -> io::Result<()> {
        if self.autoclear != 0 {
            self.file.write_all_at(
                &0u64.to_be_bytes(),
                std::mem::offset_of!(Header, autoclear_features) as u64,
            )?;
            self.autoclear = 0;
        }
        Ok(())
    }

    /// Sets the L2 entry of a guest cluster (after its data was written).
    fn set_l2_entry(&mut self, guest_cluster: u64, entry: u64) -> io::Result<()> {
        let l2_offset = self.l2_offset_for_write(guest_cluster)?;
        let index = guest_cluster % self.l2_entries();
        self.l2_table(l2_offset)?[index as usize] = entry;
        self.file
            .write_all_at(&entry.to_be_bytes(), l2_offset + index * 8)
    }

    /// The L2 table of a guest cluster, which is allocated if necessary.
    fn l2_offset_for_write(&mut self, guest_cluster: u64) -> io::Result<u64> {
        let l1_index = (guest_cluster / self.l2_entries()) as usize;
        let offset = self.l1[l1_index] & OFFSET_MASK;
        if offset != 0 {
            return Ok(offset);
        }
        let offset = self.allocate()?;
        self.l2_tables
            .insert(offset, vec![0; self.l2_entries() as usize]);
        self.l1[l1_index] = offset | COPIED;
        self.file.write_all_at(
            &(offset | COPIED).to_be_bytes(),
            self.l1_offset + l1_index as u64 * 8,
        )?;
        Ok(offset)
    }

    /// Grows the guest's disk from `old_size` to `size` bytes. The L1 table cannot move,
    /// so it must already have room for the new L1 entries in its clusters (which
    /// `qemu-img` allocates whole). How far that goes depends on the cluster size: with
    /// 64 KiB clusters (the default), it covers 4 TiB; with 4 KiB clusters, an image may
    /// not be able to grow at all.
    fn grow(&mut self, old_size: u64, size: u64) -> io::Result<()> {
        if size < old_size {
            return Err(cannot_shrink(old_size, size));
        }
        let cluster_size = self.cluster_size();
        let l2_coverage = cluster_size * self.l2_entries();
        let old_entries = self.l1.len() as u64;
        let room = (old_entries * 8).next_multiple_of(cluster_size) / 8;
        let entries = size.div_ceil(l2_coverage).max(old_entries);
        if entries > room || entries > u32::MAX as u64 {
            return Err(unsupported(format!(
                "growing the disk beyond {} bytes",
                room * l2_coverage
            )));
        }
        // The rest of the L1 table's last cluster belongs to the table, but it is not
        // necessarily zeroed: clear the new entries before the header refers to them.
        let new_entries = (entries - old_entries) as usize;
        self.file.write_all_at(
            &vec![0u8; new_entries * 8],
            self.l1_offset + old_entries * 8,
        )?;
        // We change the image like any write does (e.g., bitmaps would not match the
        // larger disk).
        self.clear_autoclear()?;
        self.file.sync_data()?;
        self.l1.resize(entries as usize, 0);
        // The L1 table grows first: a larger table than the size needs is valid, a smaller
        // one is not (in case only one of the writes reaches the disk).
        self.file.write_all_at(
            &(entries as u32).to_be_bytes(),
            std::mem::offset_of!(Header, l1_size) as u64,
        )?;
        self.file.sync_data()?;
        self.file.write_all_at(
            &size.to_be_bytes(),
            std::mem::offset_of!(Header, size) as u64,
        )?;
        self.file.sync_data()
    }
}

fn read_table(file: &File, offset: u64, entries: usize) -> io::Result<Vec<u64>> {
    let mut bytes = vec![0u8; entries * 8];
    file.read_exact_at(&mut bytes, offset)?;
    let (entries, _) = bytes.as_chunks::<8>();
    Ok(entries.iter().map(|&e| u64::from_be_bytes(e)).collect())
}

impl Image for Qcow2 {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, memory: GuestMemory, buffers: Vec<Buffer>, offset: u64) -> Io {
        let image = self.clone();
        Box::pin(unblock(move || {
            for_each_buffer(image.size, &buffers, offset, |buffer, offset| {
                image.read_blocking(&memory, buffer.address, buffer.length, offset)
            })
        }))
    }

    fn write(&self, memory: GuestMemory, buffers: Vec<Buffer>, offset: u64) -> Io {
        let image = self.clone();
        Box::pin(unblock(move || {
            for_each_buffer(image.size, &buffers, offset, |buffer, offset| {
                image.write_blocking(&memory, buffer.address, buffer.length, offset)
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

    /// Creates an empty image like `qemu-img create -f qcow2` (version 3, 64 KiB clusters).
    fn create(path: &Path, size: u64) {
        let cluster = 1u64 << 16;
        let l1_size = size.div_ceil(cluster * (cluster / 8));
        // Header, refcount table, refcount block, L1 table.
        let mut header = Header::new_zeroed();
        header.magic = QCOW2_MAGIC;
        header.version.set(3);
        header.cluster_bits.set(16);
        header.size.set(size);
        header.l1_size.set(l1_size as u32);
        header.l1_table_offset.set(3 * cluster);
        header.refcount_table_offset.set(cluster);
        header.refcount_table_clusters.set(1);
        header.refcount_order.set(4);
        header.header_length.set(size_of::<Header>() as u32);
        let file = File::create(path).unwrap();
        file.set_len(4 * cluster).unwrap();
        file.write_all_at(header.as_bytes(), 0).unwrap();
        file.write_all_at(&(2 * cluster).to_be_bytes(), cluster)
            .unwrap();
        for index in 0..4u64 {
            file.write_all_at(&1u16.to_be_bytes(), 2 * cluster + index * 2)
                .unwrap();
        }
    }

    #[test]
    fn qcow2_round_trip() {
        let _exclusive = exclusive();
        let dir = std::env::temp_dir().join(format!("midgard-qcow2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.qcow2");
        let size = 3 << 30;
        create(&path, size);

        let image = open(&path, false).unwrap();
        assert_eq!(image.size(), size);
        let mut buffer = vec![0xffu8; 4096];
        read_disk(&*image, &mut buffer, 1 << 30).unwrap();
        assert!(buffer.iter().all(|&b| b == 0));

        // Unaligned writes that cross cluster and L2 boundaries.
        let pattern: Vec<u8> = (0..200_000u32).map(|i| (i * 7 + 3) as u8).collect();
        let offsets = [
            0u64,
            65_000,
            (512 << 20) - 1000,
            size - pattern.len() as u64,
        ];
        for &offset in &offsets {
            write_disk(&*image, &pattern, offset).unwrap();
        }
        write_disk(&*image, b"overwrite", 65_536).unwrap();
        async_io::block_on(image.flush()).unwrap();
        drop(image);

        let image = open(&path, true).unwrap();
        for &offset in &offsets[..1] {
            let mut read = vec![0u8; pattern.len()];
            read_disk(&*image, &mut read, offset).unwrap();
            assert_eq!(read[..65_000], pattern[..65_000]);
        }
        let mut read = vec![0u8; pattern.len()];
        read_disk(&*image, &mut read, 65_000).unwrap();
        assert_eq!(&read[536..545], b"overwrite");
        assert_eq!(read[545..], pattern[545..]);
        for &offset in &offsets[2..] {
            read_disk(&*image, &mut read, offset).unwrap();
            assert_eq!(read, pattern);
        }
        // Untouched space still reads as zeros and takes no space.
        read_disk(&*image, &mut buffer, 2 << 30).unwrap();
        assert!(buffer.iter().all(|&b| b == 0));
        assert!(std::fs::metadata(&path).unwrap().len() < 4 << 20);
        assert!(write_disk(&*image, b"x", size).is_err());
        drop(image);
        qemu_img_check(&path);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn qcow2_concurrent_writes() {
        let _exclusive = exclusive();
        let dir = std::env::temp_dir().join(format!("midgard-qcow2-conc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.qcow2");
        create(&path, 1 << 30);
        let image = open(&path, false).unwrap();
        // 64 writes of 4 KiB into 4 new clusters (and their L2 table), all in flight at
        // once: each image operation starts when it is created.
        let chunk = 4096;
        let memory = GuestMemory::detached(64 * chunk);
        for i in 0..64 {
            memory
                .write((i * chunk) as u64, &vec![i as u8 + 1; chunk])
                .unwrap();
        }
        let writes: Vec<Io> = (0..64)
            .map(|i| {
                let address = (i * chunk) as u64;
                let buffer = Buffer {
                    address,
                    length: chunk,
                };
                image.write(memory.clone(), vec![buffer], address)
            })
            .collect();
        for write in writes {
            async_io::block_on(write).unwrap();
        }
        let mut buffer = vec![0u8; chunk];
        for i in 0..64 {
            read_disk(&*image, &mut buffer, (i * chunk) as u64).unwrap();
            assert!(buffer.iter().all(|&b| b == i as u8 + 1), "chunk {i}");
        }
        drop(image);
        qemu_img_check(&path);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn qcow2_zero_clusters() {
        let _exclusive = exclusive();
        let dir = std::env::temp_dir().join(format!("midgard-qcow2-zero-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.qcow2");
        let cluster = 1u64 << 16;
        create(&path, 1 << 30);
        let image = open(&path, false).unwrap();
        write_disk(&*image, b"data", 0).unwrap();
        drop(image);
        // Turn the first cluster into a zero cluster that keeps its host cluster (like
        // QEMU's write-zeroes does), and set an autoclear feature.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let l2_offset = read_table(&file, 3 * cluster, 1).unwrap()[0] & OFFSET_MASK;
        let entry = read_table(&file, l2_offset, 1).unwrap()[0];
        file.write_all_at(&(entry | ZERO).to_be_bytes(), l2_offset)
            .unwrap();
        let autoclear = std::mem::offset_of!(Header, autoclear_features) as u64;
        file.write_all_at(&1u64.to_be_bytes(), autoclear).unwrap();
        let file_size = file.metadata().unwrap().len();
        drop(file);

        let image = open(&path, false).unwrap();
        let mut buffer = [0xFFu8; 8];
        read_disk(&*image, &mut buffer, 0).unwrap();
        assert_eq!(buffer, [0; 8]);
        write_disk(&*image, b"x", 2).unwrap();
        read_disk(&*image, &mut buffer, 0).unwrap();
        assert_eq!(&buffer, b"\0\0x\0\0\0\0\0");
        drop(image);
        // The write reused the host cluster and cleared the autoclear features.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), file_size);
        let file = File::open(&path).unwrap();
        assert_eq!(read_table(&file, autoclear, 1).unwrap(), [0]);
        drop(file);
        qemu_img_check(&path);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn qcow2_tables_outside_of_the_file() {
        let _exclusive = exclusive();
        let dir = std::env::temp_dir().join(format!("midgard-qcow2-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.qcow2");
        create(&path, 1 << 30);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        // 2 MiB clusters and 2^16 refcount table clusters (128 GiB).
        file.write_all_at(
            &21u32.to_be_bytes(),
            std::mem::offset_of!(Header, cluster_bits) as u64,
        )
        .unwrap();
        file.write_all_at(
            &(1u32 << 16).to_be_bytes(),
            std::mem::offset_of!(Header, refcount_table_clusters) as u64,
        )
        .unwrap();
        drop(file);
        assert!(open(&path, true).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn qemu_img_check(path: &Path) {
        if let Ok(output) = std::process::Command::new("qemu-img")
            .arg("check")
            .arg(path)
            .output()
        {
            let report = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "qemu-img check: {report}");
        }
    }

    #[test]
    fn qcow2_grow() {
        let _exclusive = exclusive();
        let dir = std::env::temp_dir().join(format!("midgard-qcow2-grow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.qcow2");
        let cluster = 1u64 << 16;
        create(&path, 3 << 30);
        let image = open(&path, false).unwrap();
        write_disk(&*image, b"start", 0).unwrap();
        drop(image);
        // Garbage after the L1 table's entries (in its cluster) must not become entries.
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[0xAB; 64], 3 * cluster + 1024).unwrap();
        // An autoclear feature (e.g., bitmaps) that does not hold for the larger disk.
        let autoclear = std::mem::offset_of!(Header, autoclear_features) as u64;
        file.write_all_at(&1u64.to_be_bytes(), autoclear).unwrap();
        drop(file);

        assert!(grow(&path, 1 << 30).is_err());
        grow(&path, 64 << 30).unwrap();
        assert_eq!(size(&path).unwrap(), 64 << 30);
        let mut features = [0xFF; 8];
        File::open(&path)
            .unwrap()
            .read_exact_at(&mut features, autoclear)
            .unwrap();
        assert_eq!(features, [0; 8]);
        let image = open(&path, false).unwrap();
        let mut buffer = [0u8; 5];
        read_disk(&*image, &mut buffer, 0).unwrap();
        assert_eq!(&buffer, b"start");
        read_disk(&*image, &mut buffer, 32 << 30).unwrap();
        assert_eq!(buffer, [0; 5]);
        write_disk(&*image, b"end", (64 << 30) - 3).unwrap();
        drop(image);
        let image = open(&path, true).unwrap();
        let mut end = [0u8; 3];
        read_disk(&*image, &mut end, (64 << 30) - 3).unwrap();
        assert_eq!(&end, b"end");
        drop(image);
        qemu_img_check(&path);

        // One L1 cluster (8192 entries of 512 MiB each) covers 4 TiB.
        assert!(grow(&path, (4 << 40) + 1).is_err());
        grow(&path, 4 << 40).unwrap();
        qemu_img_check(&path);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn raw_grow() {
        let _exclusive = exclusive();
        let dir = std::env::temp_dir().join(format!("midgard-raw-grow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.ext4");
        std::fs::write(&path, b"data").unwrap();
        grow(&path, 1 << 20).unwrap();
        assert_eq!(size(&path).unwrap(), 1 << 20);
        assert!(grow(&path, 4).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
