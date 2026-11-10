//! virtio-fs: a directory of the host, mounted in the guest (`mount -t virtiofs <tag>`).
//!
//! The guest sends FUSE requests over the virtqueues; we answer them with file system
//! calls on the host. When the guest notifies a queue, the device's queue task only takes
//! the requests from it and checks them. They run on [`blocking`]'s thread pool, started by tasks on
//! the VM's executor (see [`start`]), and complete through [`Board::with_device`]. Reads
//! and writes move the data directly between the files and guest memory.
//!
//! An inode is a file of the host that we keep open (an `O_PATH` descriptor, as
//! virtiofsd does), identified by its device and inode number (such that the hard links
//! of a file are one inode). Requests work on those descriptors (through `/proc/self/fd`
//! where a system call needs a path), such that the host may rename, move or unlink
//! files that the guest uses. We only look up names in the directory that the guest
//! names and never follow a symlink, so the guest cannot reach files outside of the
//! shared directory (unless the host moves a directory that the guest knows out of it).
//! The kernel's own file systems (procfs, sysfs, ...) stay out of reach altogether (see
//! [`kernel_interface`]).
//!
//! Every inode that the guest knows is an open file: we raise our limit of those (see
//! [`raise_open_files_limit`]) and refuse new inodes before it is reached (see
//! [`Fs::max_inodes`]). Until the guest forgets a file, the host cannot unmount its file
//! system.
//!
//! The shared directory may contain other file systems of the host, whose inode numbers
//! overlap. If the driver supports it, a directory on another device than its parent is
//! announced as a submount: the guest mounts a file system of its own there (with a
//! device number of its own), such that the host's inode numbers do not clash. (A single
//! file on another device, e.g., a bind mount of one, may still clash with another.)
//!
//! Requests that use the inode table ([`Tables`]) run one at a time, under its lock;
//! reads, writes and syncs of open files run concurrently.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_int};
use std::fs::{self, File, Permissions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use async_executor::{Executor, Task};
use blocking::unblock;
use midgard_hv::GuestMemory;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::fuse::{self, ROOT_ID};
use super::{Chain, Descriptor, Device, DeviceRef, Error, InFlight, Pending, Queue, read_config};
use crate::board::Board;
use crate::host::image::Buffer;
use crate::logging::log;

const NAME: &str = "virtio-fs";
const DEVICE_ID: u32 = 26;
const QUEUE_SIZE: u16 = 256;
/// The high-priority queue and one request queue.
const NUM_REQUEST_QUEUES: u32 = 1;

/// How long the guest may cache entries and attributes (seconds). The host may change
/// files behind the guest's back, so we keep this short.
const TTL: u64 = 1;
const MAX_WRITE: u32 = 1 << 20;
/// The largest arguments of a request other than WRITE (e.g., two names, or a name and
/// a symlink's target).
const MAX_ARGS: usize = 64 * 1024;
/// How many open files we want to be allowed (virtiofsd's default): the guest may know
/// hundreds of thousands of inodes.
const OPEN_FILES: libc::rlim_t = 1_000_000;

const IN_HEADER: usize = size_of::<fuse::InHeader>();
const OUT_HEADER: usize = size_of::<fuse::OutHeader>();

/// struct virtio_fs_config.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Config {
    tag: [u8; 36],
    num_request_queues: zerocopy::little_endian::U32,
}

/// An errno, as the reply to a request.
type Errno = i32;

/// Not an errno: the request is from before the driver reset the device, so it does
/// nothing and gets no reply (see [`Fs::tables`]).
const STALE: Errno = 0;

/// Reads the arguments of a request.
struct Args<'a>(&'a [u8]);

impl<'a> Args<'a> {
    /// A request structure.
    fn get<T: FromBytes + KnownLayout + Immutable>(&mut self) -> Result<T, Errno> {
        let (value, rest) = T::read_from_prefix(self.0).map_err(|_| libc::EINVAL)?;
        self.0 = rest;
        Ok(value)
    }

    /// A NUL-terminated name of a directory entry.
    fn name(&mut self) -> Result<&'a CStr, Errno> {
        let name = self.string()?;
        let bytes = name.to_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
            return Err(libc::EINVAL);
        }
        Ok(name)
    }

    /// A NUL-terminated string (e.g., a symlink's target).
    fn string(&mut self) -> Result<&'a CStr, Errno> {
        let end = self.0.iter().position(|&b| b == 0).ok_or(libc::EINVAL)?;
        let string = CStr::from_bytes_with_nul(&self.0[..=end]).map_err(|_| libc::EINVAL)?;
        self.0 = &self.0[end + 1..];
        Ok(string)
    }
}

/// The payload of a reply.
#[derive(Default)]
struct Reply(Vec<u8>);

impl Reply {
    /// Appends a reply structure.
    fn push<T: IntoBytes + Immutable>(&mut self, value: &T) -> &mut Self {
        self.0.extend_from_slice(value.as_bytes());
        self
    }

    fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.0.extend_from_slice(value);
        self
    }
}

/// What a request put into its reply buffers.
enum Output {
    /// This payload (which still has to be written).
    Reply(Reply),
    /// This many bytes of data after the header (READ writes them directly).
    Data(usize),
}

/// The attributes of a file; `submount` announces a directory on another file system
/// of the host (see [`Fs::enter`]).
fn attr(metadata: &fs::Metadata, submount: bool) -> fuse::Attr {
    fuse::Attr {
        ino: metadata.ino(),
        size: metadata.size(),
        blocks: metadata.blocks(),
        atime: metadata.atime() as u64,
        mtime: metadata.mtime() as u64,
        ctime: metadata.ctime() as u64,
        atimensec: metadata.atime_nsec() as u32,
        mtimensec: metadata.mtime_nsec() as u32,
        ctimensec: metadata.ctime_nsec() as u32,
        mode: metadata.mode(),
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        blksize: metadata.blksize() as u32,
        flags: if submount { fuse::ATTR_SUBMOUNT } else { 0 },
    }
}

fn entry_out(id: u64, metadata: &fs::Metadata, submount: bool) -> fuse::EntryOut {
    fuse::EntryOut {
        nodeid: id,
        entry_valid: TTL,
        attr_valid: TTL,
        attr: attr(metadata, submount),
        ..Default::default()
    }
}

fn attr_out(metadata: &fs::Metadata) -> fuse::AttrOut {
    fuse::AttrOut {
        attr_valid: TTL,
        attr: attr(metadata, false),
        ..Default::default()
    }
}

fn open_out(fh: u64) -> fuse::OpenOut {
    fuse::OpenOut {
        fh,
        ..Default::default()
    }
}

fn errno(error: io::Error) -> Errno {
    error.raw_os_error().unwrap_or(libc::EIO)
}

/// The result of a system call that returns -1 on errors.
fn check(result: c_int) -> Result<(), Errno> {
    if result == -1 {
        return Err(errno(io::Error::last_os_error()));
    }
    Ok(())
}

/// The result of a system call that returns a new descriptor (or -1).
fn check_fd(result: c_int) -> Result<File, Errno> {
    check(result)?;
    // SAFETY: A new descriptor, which we own from here on.
    Ok(unsafe { File::from_raw_fd(result) })
}

/// Opens a path, without following a symlink in its last component.
fn open_path(path: &Path, flags: c_int) -> io::Result<File> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: A NUL-terminated path.
    check_fd(unsafe { libc::open(c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC) })
        .map_err(io::Error::from_raw_os_error)
}

/// The name of an open file in /proc/self/fd.
fn proc_name(file: &File) -> CString {
    CString::new(file.as_raw_fd().to_string()).unwrap()
}

/// Whether a file is on one of the kernel's own file systems (procfs, sysfs, ...), whose
/// files are interfaces of the host's kernel and not the guest's business: through them,
/// the guest would read and write our memory and environment (e.g., /proc/self/mem,
/// which we may open as it is our own process), or make the host run programs (as root).
fn kernel_interface(file: &File) -> Result<bool, Errno> {
    // (Those that libc does not know, from linux/magic.h.)
    const BINFMTFS_MAGIC: u64 = 0x42494e4d;
    const CONFIGFS_MAGIC: u64 = 0x62656570;
    const EFIVARFS_MAGIC: u64 = 0xde5e81e4;
    const FUSE_CTL_SUPER_MAGIC: u64 = 0x65735543;
    const PSTOREFS_MAGIC: u64 = 0x6165676c;
    #[allow(clippy::unnecessary_cast)]
    const KERNEL: &[u64] = &[
        libc::PROC_SUPER_MAGIC as u64,
        libc::SYSFS_MAGIC as u64,
        libc::DEBUGFS_MAGIC as u64,
        libc::TRACEFS_MAGIC as u64,
        libc::SECURITYFS_MAGIC as u64,
        libc::SELINUX_MAGIC as u64,
        libc::SMACK_MAGIC as u64,
        libc::CGROUP_SUPER_MAGIC as u64,
        libc::CGROUP2_SUPER_MAGIC as u64,
        libc::RDTGROUP_SUPER_MAGIC as u64,
        libc::BPF_FS_MAGIC as u64,
        libc::NSFS_MAGIC as u64,
        BINFMTFS_MAGIC,
        CONFIGFS_MAGIC,
        EFIVARFS_MAGIC,
        FUSE_CTL_SUPER_MAGIC,
        PSTOREFS_MAGIC,
    ];
    // SAFETY: fstatfs() writes the struct.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: An open file and the struct.
    check(unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) })?;
    Ok(KERNEL.contains(&(stat.f_type as u64)))
}

/// Raises our limit of open files to [`OPEN_FILES`] (or the system's limit), as far as
/// we may (only root may raise the hard limit); returns the limit.
fn raise_open_files_limit() -> libc::rlim_t {
    let wanted = fs::read_to_string("/proc/sys/fs/nr_open")
        .ok()
        .and_then(|nr_open| nr_open.trim().parse::<libc::rlim_t>().ok())
        .map_or(OPEN_FILES, |nr_open| nr_open.min(OPEN_FILES));
    // (Linux's usual soft limit, should getrlimit() fail.)
    let mut limit = libc::rlimit {
        rlim_cur: 1024,
        rlim_max: 1024,
    };
    // SAFETY: getrlimit() writes the struct.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == -1 || limit.rlim_cur >= wanted
    {
        return limit.rlim_cur;
    }
    let raised = libc::rlimit {
        rlim_cur: wanted,
        rlim_max: limit.rlim_max.max(wanted),
    };
    // SAFETY: setrlimit() reads the struct.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
        return wanted;
    }
    // Up to the hard limit, then.
    let current = limit.rlim_cur;
    limit.rlim_cur = limit.rlim_max;
    // SAFETY: As above.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == -1 {
        let error = io::Error::last_os_error();
        log!("{NAME}: cannot raise the limit of open files: {error}");
        return current;
    }
    limit.rlim_cur
}

// The file type bits of modes (mode_t is not u32 on all hosts).
#[allow(clippy::unnecessary_cast)]
const S_IFMT: u32 = libc::S_IFMT as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFREG: u32 = libc::S_IFREG as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFDIR: u32 = libc::S_IFDIR as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFLNK: u32 = libc::S_IFLNK as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFBLK: u32 = libc::S_IFBLK as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFCHR: u32 = libc::S_IFCHR as u32;
#[allow(clippy::unnecessary_cast)]
const S_ISGID: u32 = libc::S_ISGID as u32;

/// The device and inode number of a file.
type Key = (u64, u64);

fn key(metadata: &fs::Metadata) -> Key {
    (metadata.dev(), metadata.ino())
}

/// A file of the host that the guest knows.
struct Inode {
    key: Key,
    /// An O_PATH descriptor of the file, which keeps its inode (also when the host
    /// unlinks it).
    fd: File,
    /// The file type bits of the mode (which cannot change while we hold the file).
    kind: u32,
    /// The guest's references (a LOOKUP adds one, FORGET drops them).
    lookups: u64,
}

impl Inode {
    /// Opens (O_PATH) the file `name` in this directory, without following a symlink.
    fn child(&self, name: &CStr) -> Result<File, Errno> {
        let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: A directory descriptor and a NUL-terminated name.
        check_fd(unsafe { libc::openat(self.fd.as_raw_fd(), name.as_ptr(), flags) })
    }
}

/// A directory entry (see [`Handle::entries`]).
struct Entry {
    name: CString,
    ino: u64,
    /// The file type, as in struct dirent (d_type).
    kind: u32,
}

/// An open file or directory.
struct Handle {
    file: Arc<File>,
    /// A directory's entries, as of the last READDIR at offset 0 (None for files).
    entries: Option<Arc<Vec<Entry>>>,
}

/// The inodes and handles that the guest knows.
struct Tables {
    /// The driver generation that the tables belong to (see [`Fs::tables`]).
    generation: u64,
    /// Whether the driver mounts submounts (see [`Fs::enter`]).
    submounts: bool,
    inodes: HashMap<u64, Inode>,
    ids: HashMap<Key, u64>,
    next_id: u64,
    handles: HashMap<u64, Handle>,
    next_handle: u64,
}

impl Tables {
    /// Tables that only know the shared directory (`root`).
    fn new(generation: u64, root: Inode) -> Self {
        let root = Inode { lookups: 1, ..root };
        Self {
            generation,
            submounts: false,
            ids: HashMap::from([(root.key, ROOT_ID)]),
            inodes: HashMap::from([(ROOT_ID, root)]),
            next_id: ROOT_ID + 1,
            handles: HashMap::new(),
            next_handle: 1,
        }
    }

    /// Forgets everything but the shared directory (for the driver's `generation`).
    fn clear(&mut self, generation: u64) {
        let root = self.inodes.remove(&ROOT_ID).unwrap();
        // Node IDs and handles are not reused: a request of the old mount may still run.
        *self = Tables {
            next_id: self.next_id,
            next_handle: self.next_handle,
            ..Tables::new(generation, root)
        };
    }

    fn inode(&self, id: u64) -> Result<&Inode, Errno> {
        self.inodes.get(&id).ok_or(libc::ESTALE)
    }

    fn directory(&self, id: u64) -> Result<&Inode, Errno> {
        let inode = self.inode(id)?;
        if inode.kind != S_IFDIR {
            return Err(libc::ENOTDIR);
        }
        Ok(inode)
    }

    /// References the inode of `node` (an O_PATH descriptor, with `metadata`); returns
    /// its ID.
    fn found(&mut self, node: File, metadata: &fs::Metadata) -> u64 {
        let key = key(metadata);
        if let Some(&id) = self.ids.get(&key) {
            // (We keep the descriptor that we have; `node` is the same inode.)
            self.inodes.get_mut(&id).unwrap().lookups += 1;
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        let inode = Inode {
            key,
            fd: node,
            kind: metadata.mode() & S_IFMT,
            lookups: 1,
        };
        self.inodes.insert(id, inode);
        self.ids.insert(key, id);
        id
    }

    fn forget(&mut self, id: u64, count: u64) {
        let Some(inode) = self.inodes.get_mut(&id) else {
            return;
        };
        inode.lookups = inode.lookups.saturating_sub(count);
        if inode.lookups == 0 && id != ROOT_ID {
            let inode = self.inodes.remove(&id).unwrap();
            self.ids.remove(&inode.key);
        }
    }

    fn new_handle(&mut self, handle: Handle) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(fh, handle);
        fh
    }

    fn file(&self, fh: u64) -> Result<Arc<File>, Errno> {
        match self.handles.get(&fh) {
            Some(handle) => Ok(handle.file.clone()),
            None => Err(libc::EBADF),
        }
    }
}

/// What SETATTR (or [`Fs::created`]) changes: an open file, or an inode (its O_PATH
/// descriptor and file type).
#[derive(Clone, Copy)]
enum Target<'a> {
    File(&'a File),
    Node(&'a File, u32),
}

impl Target<'_> {
    fn metadata(self) -> Result<fs::Metadata, Errno> {
        match self {
            Target::File(file) | Target::Node(file, _) => file.metadata().map_err(errno),
        }
    }

    fn chown(self, uid: Option<u32>, gid: Option<u32>) -> Result<(), Errno> {
        match self {
            Target::File(file) => std::os::unix::fs::fchown(file, uid, gid).map_err(errno),
            Target::Node(node, _) => {
                // SAFETY: An open file and an empty path (lchown() on the file itself).
                check(unsafe {
                    libc::fchownat(
                        node.as_raw_fd(),
                        c"".as_ptr(),
                        uid.unwrap_or(u32::MAX) as libc::uid_t,
                        gid.unwrap_or(u32::MAX) as libc::gid_t,
                        libc::AT_EMPTY_PATH,
                    )
                })
            }
        }
    }

    fn chmod(self, fs: &Fs, mode: u32) -> Result<(), Errno> {
        let mode = mode & 0o7777;
        match self {
            Target::File(file) => file
                .set_permissions(Permissions::from_mode(mode))
                .map_err(errno),
            // Symlinks have no mode.
            Target::Node(_, S_IFLNK) => Err(libc::EOPNOTSUPP),
            Target::Node(node, _) => {
                let name = proc_name(node);
                // SAFETY: A directory descriptor and a NUL-terminated name.
                check(unsafe {
                    libc::fchmodat(fs.proc.as_raw_fd(), name.as_ptr(), mode as libc::mode_t, 0)
                })
            }
        }
    }

    fn truncate(self, fs: &Fs, size: u64) -> Result<(), Errno> {
        match self {
            Target::File(file) => file.set_len(size).map_err(errno),
            Target::Node(node, kind) => {
                check_regular(kind)?;
                fs.reopen(node, libc::O_WRONLY)?
                    .set_len(size)
                    .map_err(errno)
            }
        }
    }

    fn set_times(self, times: &[libc::timespec; 2]) -> Result<(), Errno> {
        match self {
            // SAFETY: An open file and two timespecs.
            Target::File(file) => {
                check(unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) })
            }
            Target::Node(node, _) => {
                // SAFETY: An open file, an empty path (the file itself) and two timespecs.
                check(unsafe {
                    libc::utimensat(
                        node.as_raw_fd(),
                        c"".as_ptr(),
                        times.as_ptr(),
                        libc::AT_EMPTY_PATH,
                    )
                })
            }
        }
    }
}

/// The host's open(2) flags for the guest's (Linux values, which are not necessarily
/// ours).
fn host_flags(flags: u32) -> c_int {
    let mut host = match flags & fuse::O_ACCMODE {
        fuse::O_WRONLY => libc::O_WRONLY,
        fuse::O_RDWR => libc::O_RDWR,
        _ => libc::O_RDONLY,
    };
    for (guest, flag) in [
        (fuse::O_APPEND, libc::O_APPEND),
        (fuse::O_TRUNC, libc::O_TRUNC),
        (fuse::O_SYNC, libc::O_SYNC),
        (fuse::O_DSYNC, libc::O_DSYNC),
    ] {
        if flags & guest == guest {
            host |= flag;
        }
    }
    host | libc::O_CLOEXEC
}

/// Checks that a mode (or its file type bits) is a regular file's, and not, e.g., a
/// device of the host, before we open the file.
fn check_regular(mode: u32) -> Result<(), Errno> {
    match mode & S_IFMT {
        S_IFREG => Ok(()),
        S_IFDIR => Err(libc::EISDIR),
        _ => Err(libc::EINVAL),
    }
}

/// The entries of an open directory, from the start.
fn list(directory: &File) -> Result<Vec<Entry>, Errno> {
    // readdir() needs a descriptor of its own (closedir() closes it).
    let fd = directory.try_clone().map_err(errno)?.into_raw_fd();
    // SAFETY: An open directory descriptor, which the stream owns from here on.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = errno(io::Error::last_os_error());
        // SAFETY: fdopendir() did not take the descriptor.
        drop(unsafe { File::from_raw_fd(fd) });
        return Err(error);
    }
    // SAFETY: An open stream (whose position the original descriptor shares).
    unsafe { libc::rewinddir(stream) };
    let mut entries = Vec::new();
    loop {
        // SAFETY: errno is ours (thread-local).
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: An open stream; the entry is valid until the next call.
        let entry = unsafe { libc::readdir64(stream) };
        if entry.is_null() {
            // The end, unless readdir() set errno.
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                // SAFETY: An open stream, which is not used afterwards.
                unsafe { libc::closedir(stream) };
                return Err(errno(error));
            }
            break;
        }
        // SAFETY: readdir() returned the entry, whose name is NUL-terminated.
        let (name, ino, kind) = unsafe {
            let entry = &*entry;
            (
                CStr::from_ptr(entry.d_name.as_ptr()).to_owned(),
                entry.d_ino,
                entry.d_type,
            )
        };
        entries.push(Entry {
            name,
            ino,
            kind: kind as u32,
        });
    }
    // SAFETY: An open stream, which is not used afterwards.
    unsafe { libc::closedir(stream) };
    Ok(entries)
}

/// Reads `file` from `offset` on into a buffer in guest memory; returns how many bytes
/// it read (fewer at the end of the file).
fn read_into(file: &File, memory: &GuestMemory, buffer: Buffer, offset: u64) -> io::Result<usize> {
    let pointer = memory
        .pointer(buffer.address, buffer.length)
        .map_err(io::Error::other)?;
    let mut done = 0;
    while done < buffer.length {
        let offset = libc::off_t::try_from(offset + done as u64)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: The buffer is inside guest memory, which `memory` keeps mapped, and the
        // kernel (not Rust) accesses it (see GuestMemory::pointer).
        let count = unsafe {
            libc::pread(
                file.as_raw_fd(),
                pointer.wrapping_add(done).cast(),
                buffer.length - done,
                offset,
            )
        };
        match count {
            0 => break,
            1.. => done += count as usize,
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
    Ok(done)
}

/// Writes a buffer in guest memory to `file` at `offset`.
fn write_from(file: &File, memory: &GuestMemory, buffer: Buffer, offset: u64) -> io::Result<()> {
    let pointer = memory
        .pointer(buffer.address, buffer.length)
        .map_err(io::Error::other)?;
    let mut done = 0;
    while done < buffer.length {
        let offset = libc::off_t::try_from(offset + done as u64)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: See read_into().
        let count = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                pointer.wrapping_add(done).cast(),
                buffer.length - done,
                offset,
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

fn total(buffers: &[Buffer]) -> usize {
    buffers.iter().map(|buffer| buffer.length).sum()
}

/// The part of `buffers` (one after the other) from `skip` bytes on, at most `length`
/// bytes.
fn slice(buffers: &[Buffer], mut skip: usize, mut length: usize) -> Vec<Buffer> {
    let mut result = Vec::new();
    for buffer in buffers {
        if length == 0 {
            break;
        }
        if skip >= buffer.length {
            skip -= buffer.length;
            continue;
        }
        let count = (buffer.length - skip).min(length);
        result.push(Buffer {
            address: buffer.address + skip as u64,
            length: count,
        });
        length -= count;
        skip = 0;
    }
    result
}

/// Reads `buffers` from guest memory.
fn read_buffers(memory: &GuestMemory, buffers: &[Buffer]) -> Result<Vec<u8>, Error> {
    let mut bytes = vec![0; total(buffers)];
    let mut offset = 0;
    for buffer in buffers {
        let end = offset + buffer.length;
        memory
            .read(buffer.address, &mut bytes[offset..end])
            .map_err(|_| Error::OutsideRam("request"))?;
        offset = end;
    }
    Ok(bytes)
}

/// Writes `data` into `buffers` (which have room for it) in guest memory.
fn write_buffers(memory: &GuestMemory, buffers: &[Buffer], data: &[u8]) -> Result<(), Error> {
    let mut offset = 0;
    for buffer in slice(buffers, 0, data.len()) {
        let end = offset + buffer.length;
        memory
            .write(buffer.address, &data[offset..end])
            .map_err(|_| Error::OutsideRam("reply"))?;
        offset = end;
    }
    Ok(())
}

/// The host side of the device: the shared directory and what the guest knows of it.
pub(crate) struct Fs {
    /// /proc/self/fd, to open the files of O_PATH descriptors (see [`Fs::reopen`]).
    proc: File,
    /// Whether new files get the user and group of the guest's process (we can only do
    /// that as root; otherwise, they are ours).
    chown: bool,
    /// How many inodes the guest may know: fewer than we may have open files, such that
    /// the guest's open files and the rest of the VMM get some too.
    max_inodes: usize,
    tables: Mutex<Tables>,
}

impl Fs {
    fn new(root: &Path) -> io::Result<Self> {
        let fd = open_path(&root.canonicalize()?, libc::O_PATH | libc::O_DIRECTORY)?;
        if kernel_interface(&fd).map_err(io::Error::from_raw_os_error)? {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let metadata = fd.metadata()?;
        let proc = open_path(Path::new("/proc/self/fd"), libc::O_PATH | libc::O_DIRECTORY)?;
        let limit = raise_open_files_limit();
        // A quarter of the descriptors stay free for everything else.
        let max_inodes = (limit - limit / 4) as usize;
        if max_inodes < 65536 {
            log!("{NAME}: the guest may only know {max_inodes} files (limit of open files)");
        }
        let root = Inode {
            key: key(&metadata),
            fd,
            kind: S_IFDIR,
            lookups: 1,
        };
        Ok(Self {
            proc,
            // SAFETY: geteuid() has no preconditions.
            chown: unsafe { libc::geteuid() } == 0,
            max_inodes,
            tables: Mutex::new(Tables::new(0, root)),
        })
    }

    /// Opens the file of an O_PATH descriptor (again) with `flags`: through
    /// /proc/self/fd, as the descriptor itself cannot be read or written.
    fn reopen(&self, node: &File, flags: c_int) -> Result<File, Errno> {
        let name = proc_name(node);
        // SAFETY: A directory descriptor and a NUL-terminated name.
        check_fd(unsafe {
            libc::openat(
                self.proc.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC,
            )
        })
    }

    /// References the inode of `node` (opened in the directory `parent`); returns the
    /// entry for the guest.
    fn enter(&self, tables: &mut Tables, parent: u64, node: File) -> Result<fuse::EntryOut, Errno> {
        let metadata = node.metadata().map_err(errno)?;
        // Another file system of the host than the parent's (a mount point, or a bind
        // mount)? Checking every crossing keeps the kernel's file systems out of reach.
        let crossing = metadata.dev() != tables.inode(parent)?.key.0;
        if crossing && kernel_interface(&node)? {
            return Err(libc::EPERM);
        }
        if !tables.ids.contains_key(&key(&metadata)) && tables.inodes.len() >= self.max_inodes {
            return Err(libc::ENFILE);
        }
        // The guest mounts another file system's directory separately, such that its
        // inode numbers do not clash with the parent's.
        let submount = tables.submounts && crossing && metadata.is_dir();
        let id = tables.found(node, &metadata);
        Ok(entry_out(id, &metadata, submount))
    }

    /// Looks up (and references) the inode of `name` in `parent`.
    fn lookup(
        &self,
        tables: &mut Tables,
        parent: u64,
        name: &CStr,
    ) -> Result<fuse::EntryOut, Errno> {
        let node = tables.directory(parent)?.child(name)?;
        self.enter(tables, parent, node)
    }

    /// Gives a file that we just created in `parent` the guest's user and group (if we
    /// can) and the mode that the guest asked for (which our umask must not reduce).
    fn created(
        &self,
        target: Target<'_>,
        parent: &Inode,
        header: &fuse::InHeader,
        mode: Option<u32>,
    ) {
        // Both are best effort: the file exists either way (and some file systems, e.g.,
        // vfat, have neither owners nor modes).
        if self.chown {
            // The file inherits the group of a set-group-ID directory.
            let inherits = parent
                .fd
                .metadata()
                .is_ok_and(|parent| parent.mode() & S_ISGID != 0);
            let gid = (!inherits).then_some(header.gid);
            let _ = target.chown(Some(header.uid), gid);
        }
        if let Some(mode) = mode {
            let _ = target.chmod(self, mode);
        }
    }

    /// Makes `name` in `parent` a hard link of `source`.
    fn link(&self, source: &Inode, parent: &Inode, name: &CStr) -> Result<(), Errno> {
        // SAFETY: Two open files, an empty path and a NUL-terminated name.
        let result = check(unsafe {
            libc::linkat(
                source.fd.as_raw_fd(),
                c"".as_ptr(),
                parent.fd.as_raw_fd(),
                name.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        });
        // Without CAP_DAC_READ_SEARCH, that is ENOENT: go through /proc/self/fd (which
        // links a symlink itself, not its target).
        match result {
            Err(libc::ENOENT) => {
                let proc_name = proc_name(&source.fd);
                // SAFETY: Two directory descriptors and two NUL-terminated names.
                check(unsafe {
                    libc::linkat(
                        self.proc.as_raw_fd(),
                        proc_name.as_ptr(),
                        parent.fd.as_raw_fd(),
                        name.as_ptr(),
                        libc::AT_SYMLINK_FOLLOW,
                    )
                })
            }
            result => result,
        }
    }

    /// Enters a file that a request just made as `name` in `parent` (`node`: its O_PATH
    /// descriptor, or the error of opening it). If that fails (e.g., for lack of
    /// descriptors), the file is removed again, such that the guest may try again.
    fn made(
        &self,
        tables: &mut Tables,
        parent: u64,
        name: &CStr,
        kind: u32,
        node: Result<File, Errno>,
    ) -> Result<fuse::EntryOut, Errno> {
        let result = node.and_then(|node| self.enter(tables, parent, node));
        if let (Err(_), Ok(parent)) = (&result, tables.directory(parent)) {
            let flags = if kind == S_IFDIR {
                libc::AT_REMOVEDIR
            } else {
                0
            };
            // SAFETY: A directory descriptor and a NUL-terminated name.
            unsafe { libc::unlinkat(parent.fd.as_raw_fd(), name.as_ptr(), flags) };
        }
        result
    }

    /// The tables, for a request of the driver's `generation`. After a reset, the first
    /// request of the new driver starts over with empty tables, and requests from before
    /// the reset (which may still run) are [`STALE`].
    fn tables(&self, generation: u64) -> Result<MutexGuard<'_, Tables>, Errno> {
        let mut tables = self.tables.lock().unwrap();
        if generation < tables.generation {
            return Err(STALE);
        }
        if generation > tables.generation {
            tables.clear(generation);
        }
        Ok(tables)
    }

    /// Runs a request of the driver's `generation`: `data` is a WRITE's data, `reply` the
    /// buffers for the reply. Returns how many bytes of reply it wrote (None for requests
    /// without a reply).
    fn execute(
        &self,
        generation: u64,
        header: &fuse::InHeader,
        args: &[u8],
        data: &[Buffer],
        reply: &[Buffer],
        memory: &GuestMemory,
    ) -> Option<u32> {
        let mut args = Args(args);
        let room = total(reply).saturating_sub(OUT_HEADER);
        let result = match header.opcode {
            fuse::FORGET | fuse::BATCH_FORGET => {
                if let Ok(mut tables) = self.tables(generation) {
                    forget(&mut tables, header, &mut args);
                }
                return None;
            }
            fuse::INTERRUPT => return None,
            fuse::READ => self
                .read(
                    generation,
                    &mut args,
                    &slice(reply, OUT_HEADER, room),
                    memory,
                )
                .map(Output::Data),
            fuse::WRITE => self.write(generation, &mut args, data, memory),
            fuse::FSYNC | fuse::FSYNCDIR => self.fsync(generation, &mut args),
            _ => self
                .tables(generation)
                .and_then(|mut tables| self.request(&mut tables, header, &mut args, room)),
        };
        let (error, payload, length) = match result {
            Ok(Output::Reply(payload)) if payload.0.len() <= room => {
                let length = payload.0.len();
                (0, payload.0, length)
            }
            Ok(Output::Reply(_)) => (-libc::ERANGE, Vec::new(), 0),
            Ok(Output::Data(length)) => (0, Vec::new(), length),
            Err(STALE) => return None,
            Err(errno) => (-errno, Vec::new(), 0),
        };
        let out_header = fuse::OutHeader {
            len: (OUT_HEADER + length) as u32,
            error,
            unique: header.unique,
        };
        let mut out = out_header.as_bytes().to_vec();
        out.extend_from_slice(&payload);
        if total(reply) < OUT_HEADER {
            log!("{NAME}: no room for the reply");
            return Some(0);
        }
        if let Err(error) = write_buffers(memory, reply, &out) {
            log!("{NAME}: {error}");
            return Some(0);
        }
        Some(out_header.len)
    }

    fn read(
        &self,
        generation: u64,
        args: &mut Args<'_>,
        buffers: &[Buffer],
        memory: &GuestMemory,
    ) -> Result<usize, Errno> {
        let read: fuse::ReadIn = args.get()?;
        let file = self.tables(generation)?.file(read.fh)?;
        let mut done = 0;
        for buffer in slice(buffers, 0, read.size as usize) {
            let count =
                read_into(&file, memory, buffer, read.offset + done as u64).map_err(errno)?;
            done += count;
            if count < buffer.length {
                break;
            }
        }
        Ok(done)
    }

    fn write(
        &self,
        generation: u64,
        args: &mut Args<'_>,
        data: &[Buffer],
        memory: &GuestMemory,
    ) -> Result<Output, Errno> {
        let write: fuse::WriteIn = args.get()?;
        let file = self.tables(generation)?.file(write.fh)?;
        let mut offset = write.offset;
        for &buffer in data {
            write_from(&file, memory, buffer, offset).map_err(errno)?;
            offset += buffer.length as u64;
        }
        let mut reply = Reply::default();
        reply.push(&fuse::WriteOut {
            size: write.size,
            padding: 0,
        });
        Ok(Output::Reply(reply))
    }

    fn fsync(&self, generation: u64, args: &mut Args<'_>) -> Result<Output, Errno> {
        let fsync: fuse::FsyncIn = args.get()?;
        let file = self.tables(generation)?.file(fsync.fh)?;
        if fsync.fsync_flags & fuse::FSYNC_FDATASYNC != 0 {
            file.sync_data()
        } else {
            file.sync_all()
        }
        .map_err(errno)?;
        Ok(Output::Reply(Reply::default()))
    }

    /// Handles a request that uses the inode table.
    fn request(
        &self,
        tables: &mut Tables,
        header: &fuse::InHeader,
        args: &mut Args<'_>,
        room: usize,
    ) -> Result<Output, Errno> {
        let id = header.nodeid;
        let mut reply = Reply::default();
        match header.opcode {
            fuse::INIT => {
                let init: fuse::InitIn = args.get()?;
                if init.major != fuse::KERNEL_VERSION {
                    return Err(libc::EPROTO);
                }
                // The driver starts over (e.g., the guest mounted the file system again).
                tables.clear(tables.generation);
                tables.submounts = init.flags & fuse::SUBMOUNTS != 0;
                let wanted = fuse::ASYNC_READ
                    | fuse::ATOMIC_O_TRUNC
                    | fuse::BIG_WRITES
                    | fuse::AUTO_INVAL_DATA
                    | fuse::DO_READDIRPLUS
                    | fuse::PARALLEL_DIROPS
                    | fuse::MAX_PAGES
                    | fuse::SUBMOUNTS;
                reply.push(&fuse::InitOut {
                    major: fuse::KERNEL_VERSION,
                    minor: fuse::KERNEL_MINOR_VERSION,
                    max_readahead: init.max_readahead,
                    flags: init.flags & wanted,
                    max_background: 64,
                    congestion_threshold: 48,
                    max_write: MAX_WRITE,
                    time_gran: 1,
                    max_pages: (MAX_WRITE / 4096) as u16,
                    ..Default::default()
                });
            }
            fuse::DESTROY => tables.clear(tables.generation),
            fuse::LOOKUP => {
                let name = args.name()?;
                reply.push(&self.lookup(tables, id, name)?);
            }
            fuse::GETATTR => {
                let getattr: fuse::GetattrIn = args.get().unwrap_or_default();
                let metadata = if getattr.getattr_flags & fuse::GETATTR_FH != 0 {
                    tables.file(getattr.fh)?.metadata()
                } else {
                    tables.inode(id)?.fd.metadata()
                }
                .map_err(errno)?;
                reply.push(&attr_out(&metadata));
            }
            fuse::SETATTR => {
                let setattr: fuse::SetattrIn = args.get()?;
                let metadata = self.setattr(tables, id, &setattr)?;
                reply.push(&attr_out(&metadata));
            }
            fuse::READLINK => {
                let inode = tables.inode(id)?;
                if inode.kind != S_IFLNK {
                    return Err(libc::EINVAL);
                }
                // (Linux does not make longer symlinks.)
                let mut target = vec![0u8; libc::PATH_MAX as usize];
                // SAFETY: An open symlink, an empty path (the symlink itself) and a
                // buffer of the given length.
                let length = unsafe {
                    libc::readlinkat(
                        inode.fd.as_raw_fd(),
                        c"".as_ptr(),
                        target.as_mut_ptr().cast(),
                        target.len(),
                    )
                };
                if length == -1 {
                    return Err(errno(io::Error::last_os_error()));
                }
                reply.bytes(&target[..length as usize]);
            }
            fuse::SYMLINK => {
                let name = args.name()?;
                let target = args.string()?;
                let parent = tables.directory(id)?;
                // SAFETY: A directory descriptor and two NUL-terminated strings.
                check(unsafe {
                    libc::symlinkat(target.as_ptr(), parent.fd.as_raw_fd(), name.as_ptr())
                })?;
                let node = parent.child(name).inspect(|node| {
                    self.created(Target::Node(node, S_IFLNK), parent, header, None);
                });
                reply.push(&self.made(tables, id, name, S_IFLNK, node)?);
            }
            fuse::MKNOD => {
                let mknod: fuse::MknodIn = args.get()?;
                let name = args.name()?;
                let parent = tables.directory(id)?;
                let mode = mknod.mode & !mknod.umask;
                let kind = mode & S_IFMT;
                // Devices of the host are not the guest's business.
                if matches!(kind, S_IFBLK | S_IFCHR) {
                    return Err(libc::EPERM);
                }
                // (Also regular files, which mknod() makes without privileges.)
                // SAFETY: A directory descriptor and a NUL-terminated name.
                check(unsafe {
                    libc::mknodat(
                        parent.fd.as_raw_fd(),
                        name.as_ptr(),
                        mode as libc::mode_t,
                        mknod.rdev as libc::dev_t,
                    )
                })?;
                let node = parent.child(name).inspect(|node| {
                    self.created(Target::Node(node, kind), parent, header, Some(mode));
                });
                reply.push(&self.made(tables, id, name, kind, node)?);
            }
            fuse::MKDIR => {
                let mkdir: fuse::MkdirIn = args.get()?;
                let name = args.name()?;
                let parent = tables.directory(id)?;
                let mode = mkdir.mode & !mkdir.umask & 0o7777;
                // SAFETY: A directory descriptor and a NUL-terminated name.
                check(unsafe {
                    libc::mkdirat(parent.fd.as_raw_fd(), name.as_ptr(), mode as libc::mode_t)
                })?;
                let node = parent.child(name).and_then(|node| {
                    // Keep the set-group-ID bit that the directory inherits from its parent.
                    let inherited = node.metadata().map_err(errno)?.mode() & S_ISGID;
                    let target = Target::Node(&node, S_IFDIR);
                    self.created(target, parent, header, Some(mode | inherited));
                    Ok(node)
                });
                reply.push(&self.made(tables, id, name, S_IFDIR, node)?);
            }
            fuse::UNLINK | fuse::RMDIR => {
                let name = args.name()?;
                let parent = tables.directory(id)?;
                let flags = if header.opcode == fuse::RMDIR {
                    libc::AT_REMOVEDIR
                } else {
                    0
                };
                // SAFETY: A directory descriptor and a NUL-terminated name.
                check(unsafe { libc::unlinkat(parent.fd.as_raw_fd(), name.as_ptr(), flags) })?;
            }
            fuse::RENAME | fuse::RENAME2 => {
                let (new_parent, flags) = if header.opcode == fuse::RENAME2 {
                    let rename: fuse::Rename2In = args.get()?;
                    (rename.newdir, rename.flags)
                } else {
                    (args.get::<fuse::RenameIn>()?.newdir, 0)
                };
                // We cannot make whiteouts.
                if flags & !(fuse::RENAME_NOREPLACE | fuse::RENAME_EXCHANGE) != 0 {
                    return Err(libc::EINVAL);
                }
                let old_name = args.name()?;
                let new_name = args.name()?;
                let from = tables.directory(id)?;
                let to = tables.directory(new_parent)?;
                // SAFETY: Two directory descriptors and two NUL-terminated names.
                check(unsafe {
                    libc::renameat2(
                        from.fd.as_raw_fd(),
                        old_name.as_ptr(),
                        to.fd.as_raw_fd(),
                        new_name.as_ptr(),
                        flags as libc::c_uint,
                    )
                })?;
            }
            fuse::LINK => {
                let link: fuse::LinkIn = args.get()?;
                let name = args.name()?;
                let source = tables.inode(link.oldnodeid)?;
                let parent = tables.directory(id)?;
                self.link(source, parent, name)?;
                reply.push(&self.lookup(tables, id, name)?);
            }
            fuse::OPEN => {
                let open: fuse::OpenIn = args.get()?;
                let inode = tables.inode(id)?;
                if inode.kind != S_IFREG {
                    return Err(libc::EINVAL);
                }
                let file = self.reopen(&inode.fd, host_flags(open.flags))?;
                let handle = Handle {
                    file: Arc::new(file),
                    entries: None,
                };
                reply.push(&open_out(tables.new_handle(handle)));
            }
            fuse::CREATE => {
                let create: fuse::CreateIn = args.get()?;
                let name = args.name()?;
                let parent = tables.directory(id)?;
                let mode = create.mode & !create.umask;
                let flags = host_flags(create.flags);
                // SAFETY: A directory descriptor and a NUL-terminated name.
                let created = check_fd(unsafe {
                    libc::openat(
                        parent.fd.as_raw_fd(),
                        name.as_ptr(),
                        flags | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                        (mode & 0o7777) as libc::c_uint,
                    )
                });
                let (file, entry) = match created {
                    // Only a file that we create gets the guest's user and mode.
                    Ok(file) => {
                        self.created(Target::File(&file), parent, header, Some(mode));
                        let node = self.reopen(&file, libc::O_PATH);
                        let entry = self.made(tables, id, name, S_IFREG, node)?;
                        (file, entry)
                    }
                    Err(libc::EEXIST) if create.flags & fuse::O_EXCL == 0 => {
                        let node = parent.child(name)?;
                        check_regular(node.metadata().map_err(errno)?.mode())?;
                        let file = self.reopen(&node, flags)?;
                        let entry = self.enter(tables, id, node)?;
                        (file, entry)
                    }
                    Err(error) => return Err(error),
                };
                let handle = Handle {
                    file: Arc::new(file),
                    entries: None,
                };
                reply
                    .push(&entry)
                    .push(&open_out(tables.new_handle(handle)));
            }
            fuse::STATFS => {
                let inode = tables.inode(id)?;
                // SAFETY: fstatvfs() writes the struct.
                let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
                // SAFETY: An open file and the struct.
                check(unsafe { libc::fstatvfs(inode.fd.as_raw_fd(), &mut stat) })?;
                reply.push(&fuse::StatfsOut {
                    blocks: stat.f_blocks as u64,
                    bfree: stat.f_bfree as u64,
                    bavail: stat.f_bavail as u64,
                    files: stat.f_files as u64,
                    ffree: stat.f_ffree as u64,
                    bsize: stat.f_bsize as u32,
                    namelen: stat.f_namemax as u32,
                    frsize: stat.f_frsize as u32,
                    ..Default::default()
                });
            }
            fuse::RELEASE | fuse::RELEASEDIR => {
                let release: fuse::HandleIn = args.get()?;
                tables.handles.remove(&release.fh);
            }
            fuse::FLUSH => {}
            fuse::OPENDIR => {
                let inode = tables.directory(id)?;
                let file = self.reopen(&inode.fd, libc::O_RDONLY | libc::O_DIRECTORY)?;
                // The entries are read at offset 0.
                let handle = Handle {
                    file: Arc::new(file),
                    entries: Some(Arc::default()),
                };
                reply.push(&open_out(tables.new_handle(handle)));
            }
            fuse::READDIR | fuse::READDIRPLUS => {
                let read: fuse::ReadIn = args.get()?;
                let size = (read.size as usize).min(room);
                let Some(Handle {
                    file,
                    entries: Some(entries),
                }) = tables.handles.get(&read.fh)
                else {
                    return Err(libc::EBADF);
                };
                let (file, mut entries) = (file.clone(), entries.clone());
                // At the start (also after rewinddir(), and on a new handle), the entries
                // are those that the directory has now.
                if read.offset == 0 || entries.is_empty() {
                    entries = Arc::new(list(&file)?);
                    let handle = tables.handles.get_mut(&read.fh).unwrap();
                    handle.entries = Some(entries.clone());
                }
                let plus = header.opcode == fuse::READDIRPLUS;
                let start = usize::try_from(read.offset).unwrap_or(usize::MAX);
                for (index, entry) in entries.iter().enumerate().skip(start) {
                    let name = entry.name.to_bytes();
                    let dirent = size_of::<fuse::Dirent>() + name.len();
                    let length = dirent.next_multiple_of(8)
                        + if plus { size_of::<fuse::EntryOut>() } else { 0 };
                    if reply.0.len() + length > size {
                        break;
                    }
                    if plus {
                        let lookup = match name {
                            b"." | b".." => None,
                            _ => self.lookup(tables, id, &entry.name).ok(),
                        };
                        // Without an entry (node ID 0), the guest does not cache one.
                        reply.push(&lookup.unwrap_or_default());
                    }
                    reply
                        .push(&fuse::Dirent {
                            ino: entry.ino,
                            off: (index + 1) as u64,
                            namelen: name.len() as u32,
                            type_: entry.kind,
                        })
                        .bytes(name);
                    reply.0.resize(reply.0.len().next_multiple_of(8), 0);
                }
            }
            fuse::ACCESS => {
                let access: fuse::AccessIn = args.get()?;
                let name = proc_name(&tables.inode(id)?.fd);
                // SAFETY: A directory descriptor and a NUL-terminated name.
                check(unsafe {
                    libc::faccessat(
                        self.proc.as_raw_fd(),
                        name.as_ptr(),
                        access.mask as c_int,
                        0,
                    )
                })?;
            }
            _ => return Err(libc::ENOSYS),
        }
        Ok(Output::Reply(reply))
    }

    /// Applies the attributes of a SETATTR request; returns the file's new attributes.
    fn setattr(
        &self,
        tables: &Tables,
        id: u64,
        setattr: &fuse::SetattrIn,
    ) -> Result<fs::Metadata, Errno> {
        let valid = setattr.valid;
        let inode = tables.inode(id)?;
        // Devices of the host are not the guest's business (see MKNOD).
        if matches!(inode.kind, S_IFBLK | S_IFCHR) {
            return Err(libc::EPERM);
        }
        let file;
        let target = if valid & fuse::FATTR_FH != 0 {
            file = tables.file(setattr.fh)?;
            Target::File(&file)
        } else {
            Target::Node(&inode.fd, inode.kind)
        };
        // Before the mode, as chown() may clear the set-user-ID bit.
        if valid & (fuse::FATTR_UID | fuse::FATTR_GID) != 0 {
            let uid = (valid & fuse::FATTR_UID != 0).then_some(setattr.uid);
            let gid = (valid & fuse::FATTR_GID != 0).then_some(setattr.gid);
            target.chown(uid, gid)?;
        }
        if valid & fuse::FATTR_MODE != 0 {
            target.chmod(self, setattr.mode)?;
        }
        if valid & fuse::FATTR_SIZE != 0 {
            target.truncate(self, setattr.size)?;
        }
        if valid & (fuse::FATTR_ATIME | fuse::FATTR_MTIME) != 0 {
            let time = |set: u32, now: u32, sec: u64, nsec: u32| libc::timespec {
                tv_sec: sec as libc::time_t,
                tv_nsec: if valid & set == 0 {
                    libc::UTIME_OMIT
                } else if valid & now != 0 {
                    libc::UTIME_NOW
                } else {
                    nsec as _
                },
            };
            target.set_times(&[
                time(
                    fuse::FATTR_ATIME,
                    fuse::FATTR_ATIME_NOW,
                    setattr.atime,
                    setattr.atimensec,
                ),
                time(
                    fuse::FATTR_MTIME,
                    fuse::FATTR_MTIME_NOW,
                    setattr.mtime,
                    setattr.mtimensec,
                ),
            ])?;
        }
        target.metadata()
    }
}

/// Handles FORGET and BATCH_FORGET.
fn forget(tables: &mut Tables, header: &fuse::InHeader, args: &mut Args<'_>) {
    if header.opcode == fuse::FORGET {
        if let Ok(forget) = args.get::<fuse::ForgetIn>() {
            tables.forget(header.nodeid, forget.nlookup);
        }
        return;
    }
    if let Ok(batch) = args.get::<fuse::BatchForgetIn>() {
        for _ in 0..batch.count {
            let Ok(one) = args.get::<fuse::ForgetOne>() else {
                break;
            };
            tables.forget(one.nodeid, one.nlookup);
        }
    }
}

/// The buffers of descriptors.
fn buffers<'a>(descriptors: impl Iterator<Item = &'a Descriptor>) -> Vec<Buffer> {
    let buffer = |d: &Descriptor| Buffer {
        address: d.address,
        length: d.length as usize,
    };
    descriptors.map(buffer).collect()
}

/// Answers a request that we cannot run with `errno` (if it has a reply, and room for
/// one); returns the length of the reply.
fn reject(chain: &Chain, memory: &GuestMemory, errno: Errno) -> u32 {
    let readable = buffers(chain.readable());
    let Ok(header) = read_buffers(memory, &slice(&readable, 0, IN_HEADER)) else {
        return 0;
    };
    let Ok(header) = fuse::InHeader::read_from_bytes(&header) else {
        return 0;
    };
    if !has_reply(header.opcode) || chain.writable_len() < OUT_HEADER {
        return 0;
    }
    let out = fuse::OutHeader {
        len: OUT_HEADER as u32,
        error: -errno,
        unique: header.unique,
    };
    chain.write(memory, out.as_bytes())
}

/// Whether the guest expects a reply to a request.
fn has_reply(opcode: u32) -> bool {
    !matches!(opcode, fuse::FORGET | fuse::BATCH_FORGET | fuse::INTERRUPT)
}

pub(crate) struct VirtioFs {
    tag: String,
    /// Requests for the device's task (see [`start`]).
    requests: async_channel::Sender<Request>,
    /// The requests that were sent and are not completed (or dropped) yet.
    in_flight: InFlight,
    /// Counts the driver's resets: requests from before a reset do nothing and are not
    /// returned to the driver.
    generation: Arc<AtomicU64>,
}

/// The host side of a virtio-fs device (see [`start`]).
pub(crate) struct Backend {
    fs: Arc<Fs>,
    requests: async_channel::Receiver<Request>,
    /// See [`VirtioFs::generation`].
    generation: Arc<AtomicU64>,
}

/// A request, taken from a queue.
pub(crate) struct Request {
    /// Counts the request until it is completed (see [`VirtioFs::in_flight`]).
    pending: Pending,
    queue: usize,
    head: u16,
    generation: u64,
    header: fuse::InHeader,
    /// The arguments after the header (for WRITE, without the data).
    args: Vec<u8>,
    /// WRITE: the data.
    data: Vec<Buffer>,
    /// The buffers for the reply.
    reply: Vec<Buffer>,
}

/// A request that is done.
struct Completion {
    /// Ends the request's count when the completion is dropped.
    _pending: Pending,
    queue: usize,
    head: u16,
    generation: u64,
    /// The length of the reply.
    written: u32,
}

impl Request {
    /// Runs the request on `fs`, unless the driver reset the device since it sent the
    /// request (`generation` is the current one). Returns the completion, if the request
    /// has a reply.
    fn execute(self, fs: &Fs, memory: &GuestMemory, generation: &AtomicU64) -> Option<Completion> {
        if self.generation != generation.load(Ordering::SeqCst) {
            return None;
        }
        let written = fs.execute(
            self.generation,
            &self.header,
            &self.args,
            &self.data,
            &self.reply,
            memory,
        )?;
        Some(Completion {
            _pending: self.pending,
            queue: self.queue,
            head: self.head,
            generation: self.generation,
            written,
        })
    }
}

impl VirtioFs {
    /// The device for the host directory `root` (`mount -t virtiofs <tag>` in the guest),
    /// and its host side (for [`start`]).
    pub(crate) fn new(tag: &str, root: &Path) -> io::Result<(Self, Backend)> {
        let fs = Arc::new(Fs::new(root)?);
        let (requests, receiver) = async_channel::unbounded();
        let generation = Arc::new(AtomicU64::new(0));
        let device = Self {
            tag: tag.to_string(),
            requests,
            in_flight: InFlight::default(),
            generation: generation.clone(),
        };
        let backend = Backend {
            fs,
            requests: receiver,
            generation,
        };
        Ok((device, backend))
    }

    /// Takes a request from a chain (its arguments; a WRITE's data stays in guest memory).
    fn check(&self, queue: usize, chain: &Chain, memory: &GuestMemory) -> Result<Request, Error> {
        let readable = buffers(chain.readable());
        let reply = buffers(chain.writable());
        for buffer in readable.iter().chain(&reply) {
            memory
                .check_range(buffer.address, buffer.length)
                .map_err(|_| Error::OutsideRam("buffer"))?;
        }
        let header = read_buffers(memory, &slice(&readable, 0, IN_HEADER))?;
        let header = fuse::InHeader::read_from_bytes(&header)
            .map_err(|_| Error::Malformed("request without a header"))?;
        let (args, data) = if header.opcode == fuse::WRITE {
            let args = read_buffers(
                memory,
                &slice(&readable, IN_HEADER, size_of::<fuse::WriteIn>()),
            )?;
            let write = fuse::WriteIn::read_from_bytes(&args)
                .map_err(|_| Error::Malformed("WRITE without its arguments"))?;
            if write.size > MAX_WRITE {
                return Err(Error::TooLarge(write.size as u64));
            }
            let start = IN_HEADER + size_of::<fuse::WriteIn>();
            let data = slice(&readable, start, write.size as usize);
            if total(&data) != write.size as usize {
                return Err(Error::Malformed("WRITE without its data"));
            }
            (args, data)
        } else {
            let length = total(&readable).saturating_sub(IN_HEADER);
            if length > MAX_ARGS {
                return Err(Error::TooLarge(length as u64));
            }
            let args = read_buffers(memory, &slice(&readable, IN_HEADER, length))?;
            (args, Vec::new())
        };
        Ok(Request {
            pending: self.in_flight.start(),
            queue,
            head: chain.head,
            generation: self.generation.load(Ordering::SeqCst),
            header,
            args,
            data,
            reply,
        })
    }

    /// Returns a request that is done to the driver; returns whether it did (it does not
    /// if the driver reset the device in the meantime).
    fn complete(&self, completion: Completion, queues: &mut [Queue], memory: &GuestMemory) -> bool {
        if completion.generation != self.generation.load(Ordering::SeqCst) {
            return false;
        }
        queues[completion.queue].push(memory, completion.head, completion.written);
        true
    }
}

/// Lets the VM wait for a virtio-fs device's requests when it stops (see [`start`]).
pub(crate) struct Shutdown {
    requests: async_channel::Receiver<Request>,
    task: Task<()>,
}

impl Shutdown {
    /// Waits for all requests (the vCPUs must be stopped, such that there are no new
    /// ones).
    pub(crate) async fn wait(self) {
        self.requests.close();
        self.task.await;
    }
}

/// Runs the requests of `device` on [`blocking`]'s thread pool, started by tasks on
/// `executor` (requests run concurrently); they complete through `board`.
pub(crate) fn start(
    executor: &Arc<Executor<'static>>,
    board: Arc<Board>,
    device: DeviceRef<VirtioFs>,
    backend: Backend,
) -> Shutdown {
    let Backend {
        fs,
        requests,
        generation,
    } = backend;
    // The executor owns this task, which must not keep it alive.
    let tasks: Weak<Executor<'static>> = Arc::downgrade(executor);
    let receiver = requests.clone();
    let task = executor.spawn(async move {
        let mut running: Vec<Task<()>> = Vec::new();
        while let Ok(request) = receiver.recv().await {
            let Some(executor) = tasks.upgrade() else {
                break;
            };
            running.retain(|task| !task.is_finished());
            let (fs, board, generation) = (fs.clone(), board.clone(), generation.clone());
            running.push(executor.spawn(async move {
                let memory = board.memory.clone();
                let completion = unblock(move || request.execute(&fs, &memory, &generation)).await;
                if let Some(completion) = completion {
                    board.with_device(device, |fs, queues, memory| {
                        ((), fs.complete(completion, queues, memory))
                    });
                }
            }));
        }
        // The channel was closed (see Shutdown::wait()).
        for task in running {
            task.await;
        }
    });
    Shutdown { requests, task }
}

impl Device for VirtioFs {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![QUEUE_SIZE; 1 + NUM_REQUEST_QUEUES as usize]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = Config {
            tag: [0; 36],
            num_request_queues: NUM_REQUEST_QUEUES.into(),
        };
        let tag = &self.tag.as_bytes()[..self.tag.len().min(config.tag.len())];
        config.tag[..tag.len()].copy_from_slice(tag);
        read_config(config.as_bytes(), offset, data);
    }

    fn process(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) -> bool {
        let mut used = false;
        while let Some(chain) = queues[queue].next_chain(memory, NAME) {
            let written = match self.check(queue, &chain, memory) {
                Ok(request) => {
                    let reply = has_reply(request.header.opcode);
                    match self.requests.try_send(request) {
                        Ok(()) if reply => continue,
                        // Requests without a reply (e.g., FORGET) are done once we have
                        // their arguments.
                        Ok(()) => 0,
                        // The task is only gone when the VM stops.
                        Err(_) => reject(&chain, memory, libc::EIO),
                    }
                }
                Err(error) => {
                    log!("{NAME}: {error}");
                    let errno = match error {
                        Error::TooLarge(_) => libc::E2BIG,
                        _ => libc::EINVAL,
                    };
                    reject(&chain, memory, errno)
                }
            };
            queues[queue].push(memory, chain.head, written);
            used = true;
        }
        used
    }

    fn reset(&mut self) {
        // Requests from before the reset do nothing, and they are not returned to the
        // driver. The reset completes when those that run are done (see in_flight()).
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn in_flight(&self) -> Option<InFlight> {
        Some(self.in_flight.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    use super::super::tests::{USED, descriptor, memory, offer, queue};
    use super::super::{DESC_NEXT, DESC_WRITE};
    use super::*;

    /// Where requests put a WRITE's data.
    const DATA: u64 = 0x4000;
    /// The reply buffer.
    const REPLY: u64 = 0x8000;
    const REPLY_SIZE: usize = 0x8000;

    /// A shared directory in a fresh temporary directory, and guest memory.
    struct Test {
        dir: PathBuf,
        /// Whether the test made the directory (and removes it).
        owned: bool,
        fs: Fs,
        memory: GuestMemory,
        generation: u64,
    }

    impl Drop for Test {
        fn drop(&mut self) {
            if self.owned {
                let _ = fs::remove_dir_all(&self.dir);
            }
        }
    }

    fn name(name: &str) -> Vec<u8> {
        let mut bytes = name.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    impl Test {
        fn new() -> Self {
            static COUNT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "midgard-fs-test-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir(&dir).unwrap();
            Self::with_root(dir, true, 0)
        }

        /// A test that shares `dir` (and removes it afterwards if `owned`), with the
        /// driver's INIT `flags`.
        fn with_root(dir: PathBuf, owned: bool, flags: u32) -> Self {
            let fs = Fs::new(&dir).unwrap();
            let mut test = Self {
                dir,
                owned,
                fs,
                memory: GuestMemory::detached(0x10000),
                generation: 0,
            };
            let init = fuse::InitIn {
                major: fuse::KERNEL_VERSION,
                minor: fuse::KERNEL_MINOR_VERSION,
                max_readahead: 0,
                flags,
            };
            test.call(fuse::INIT, 0, init.as_bytes(), &[]).unwrap();
            test
        }

        /// Runs a request; returns the reply's payload or the error.
        fn call(
            &mut self,
            opcode: u32,
            nodeid: u64,
            args: &[u8],
            data: &[u8],
        ) -> Result<Vec<u8>, i32> {
            let header = fuse::InHeader {
                opcode,
                unique: 7,
                nodeid,
                ..Default::default()
            };
            self.memory.write(DATA, data).unwrap();
            let data = [Buffer {
                address: DATA,
                length: data.len(),
            }];
            let reply = [Buffer {
                address: REPLY,
                length: REPLY_SIZE,
            }];
            let written = self
                .fs
                .execute(self.generation, &header, args, &data, &reply, &self.memory)
                .expect("no reply");
            let mut out = vec![0; written as usize];
            self.memory.read(REPLY, &mut out).unwrap();
            let (out_header, payload) = fuse::OutHeader::read_from_prefix(&out).unwrap();
            assert_eq!(out_header.len, written);
            assert_eq!(out_header.unique, 7);
            match out_header.error {
                0 => Ok(payload.to_vec()),
                error => Err(-error),
            }
        }

        fn lookup(&mut self, parent: u64, child: &str) -> Result<fuse::EntryOut, i32> {
            let reply = self.call(fuse::LOOKUP, parent, &name(child), &[])?;
            Ok(fuse::EntryOut::read_from_bytes(&reply).unwrap())
        }

        /// Drops `count` references of the guest to `id`.
        fn forget(&mut self, id: u64, count: u64) {
            let header = fuse::InHeader {
                opcode: fuse::FORGET,
                nodeid: id,
                ..Default::default()
            };
            let args = fuse::ForgetIn { nlookup: count };
            let reply = self.fs.execute(
                self.generation,
                &header,
                args.as_bytes(),
                &[],
                &[],
                &self.memory,
            );
            assert_eq!(reply, None);
        }

        /// Creates a file with `mode` (and no umask); returns its node ID and handle.
        fn create(&mut self, parent: u64, child: &str, mode: u32) -> Result<(u64, u64), i32> {
            let create = fuse::CreateIn {
                flags: fuse::O_RDWR,
                mode: S_IFREG | mode,
                umask: 0,
                open_flags: 0,
            };
            let mut args = create.as_bytes().to_vec();
            args.extend(name(child));
            let reply = self.call(fuse::CREATE, parent, &args, &[])?;
            let (entry, open) = fuse::EntryOut::read_from_prefix(&reply).unwrap();
            Ok((
                entry.nodeid,
                fuse::OpenOut::read_from_bytes(open).unwrap().fh,
            ))
        }

        fn mkdir(&mut self, parent: u64, child: &str) -> u64 {
            let mkdir = fuse::MkdirIn {
                mode: 0o755,
                umask: 0,
            };
            let mut args = mkdir.as_bytes().to_vec();
            args.extend(name(child));
            let reply = self.call(fuse::MKDIR, parent, &args, &[]).unwrap();
            fuse::EntryOut::read_from_bytes(&reply).unwrap().nodeid
        }

        fn getattr(&mut self, id: u64, fh: Option<u64>) -> Result<fuse::Attr, i32> {
            let getattr = fuse::GetattrIn {
                getattr_flags: if fh.is_some() { fuse::GETATTR_FH } else { 0 },
                dummy: 0,
                fh: fh.unwrap_or(0),
            };
            let reply = self.call(fuse::GETATTR, id, getattr.as_bytes(), &[])?;
            Ok(fuse::AttrOut::read_from_bytes(&reply).unwrap().attr)
        }

        fn write(&mut self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
            let write = fuse::WriteIn {
                fh,
                offset,
                size: data.len() as u32,
                ..Default::default()
            };
            let reply = self.call(fuse::WRITE, 0, write.as_bytes(), data)?;
            Ok(fuse::WriteOut::read_from_bytes(&reply).unwrap().size)
        }

        fn read(
            &mut self,
            opcode: u32,
            id: u64,
            fh: u64,
            offset: u64,
            size: u32,
        ) -> Result<Vec<u8>, i32> {
            let read = fuse::ReadIn {
                fh,
                offset,
                size,
                ..Default::default()
            };
            self.call(opcode, id, read.as_bytes(), &[])
        }

        fn unlink(&mut self, parent: u64, child: &str) -> Result<(), i32> {
            self.call(fuse::UNLINK, parent, &name(child), &[]).map(drop)
        }

        fn rename(&mut self, from: (u64, &str), to: (u64, &str), flags: u32) -> Result<(), i32> {
            let rename = fuse::Rename2In {
                newdir: to.0,
                flags,
                padding: 0,
            };
            let mut args = rename.as_bytes().to_vec();
            args.extend(name(from.1));
            args.extend(name(to.1));
            self.call(fuse::RENAME2, from.0, &args, &[]).map(drop)
        }

        /// The names in a READDIR(PLUS) reply, and the node IDs of READDIRPLUS.
        fn names(reply: &[u8], plus: bool) -> Vec<(String, u64)> {
            let mut names = Vec::new();
            let mut rest = reply;
            while !rest.is_empty() {
                let mut id = 0;
                if plus {
                    let (entry, tail) = fuse::EntryOut::read_from_prefix(rest).unwrap();
                    id = entry.nodeid;
                    rest = tail;
                }
                let (dirent, tail) = fuse::Dirent::read_from_prefix(rest).unwrap();
                let length = dirent.namelen as usize;
                names.push((String::from_utf8(tail[..length].to_vec()).unwrap(), id));
                rest = &tail[length.next_multiple_of(8)..];
            }
            names
        }

        /// How many inodes the guest knows.
        fn inodes(&self) -> usize {
            self.fs.tables.lock().unwrap().inodes.len()
        }
    }

    #[test]
    fn create_write_read() {
        let mut test = Test::new();
        let (id, fh) = test.create(ROOT_ID, "file", 0o666).unwrap();
        assert_eq!(test.write(fh, 0, b"hello").unwrap(), 5);
        assert_eq!(test.write(fh, 5, b", world").unwrap(), 7);
        assert_eq!(
            test.read(fuse::READ, id, fh, 0, 100).unwrap(),
            b"hello, world"
        );
        assert_eq!(test.read(fuse::READ, id, fh, 7, 3).unwrap(), b"wor");
        assert_eq!(test.read(fuse::READ, id, fh, 100, 3).unwrap(), b"");
        let attr = test.getattr(id, None).unwrap();
        assert_eq!(attr.size, 12);
        // Our umask does not apply.
        assert_eq!(attr.mode, S_IFREG | 0o666);
        // The guest sees the host's inode numbers.
        assert_eq!(attr.ino, fs::metadata(test.dir.join("file")).unwrap().ino());
        assert_eq!(attr.flags, 0);
        assert_eq!(fs::read(test.dir.join("file")).unwrap(), b"hello, world");
        // CREATE of an existing file opens it.
        let (again, _) = test.create(ROOT_ID, "file", 0o600).unwrap();
        assert_eq!(again, id);
        assert_eq!(test.getattr(id, None).unwrap().mode, S_IFREG | 0o666);
        test.call(fuse::RELEASE, id, fh.as_bytes(), &[]).unwrap();
        assert_eq!(test.read(fuse::READ, id, fh, 0, 100), Err(libc::EBADF));
    }

    #[test]
    fn unlinked_files() {
        let mut test = Test::new();
        let (id, fh) = test.create(ROOT_ID, "file", 0o644).unwrap();
        test.write(fh, 0, b"data").unwrap();
        test.unlink(ROOT_ID, "file").unwrap();
        // The file still works without its name (also without its handle, as Linux does
        // for fstat() and fchmod()).
        assert_eq!(test.getattr(id, Some(fh)).unwrap().size, 4);
        assert_eq!(test.getattr(id, None).unwrap().size, 4);
        let setattr = fuse::SetattrIn {
            valid: fuse::FATTR_MODE,
            mode: 0o600,
            ..Default::default()
        };
        let reply = test
            .call(fuse::SETATTR, id, setattr.as_bytes(), &[])
            .unwrap();
        let attr = fuse::AttrOut::read_from_bytes(&reply).unwrap().attr;
        assert_eq!(attr.mode, S_IFREG | 0o600);
        assert_eq!(test.read(fuse::READ, id, fh, 0, 100).unwrap(), b"data");
        // A new file of that name is a new inode.
        let (new, _) = test.create(ROOT_ID, "file", 0o644).unwrap();
        assert_ne!(new, id);
        assert_eq!(test.getattr(new, None).unwrap().size, 0);
        // The inode lives as long as the guest knows it.
        test.call(fuse::RELEASE, id, fh.as_bytes(), &[]).unwrap();
        assert_eq!(test.getattr(id, None).unwrap().nlink, 0);
        test.forget(id, 1);
        assert_eq!(test.getattr(id, None), Err(libc::ESTALE));
    }

    #[test]
    fn hard_links_are_one_inode() {
        let mut test = Test::new();
        let (id, _) = test.create(ROOT_ID, "a", 0o644).unwrap();
        let mut args = fuse::LinkIn { oldnodeid: id }.as_bytes().to_vec();
        args.extend(name("b"));
        let reply = test.call(fuse::LINK, ROOT_ID, &args, &[]).unwrap();
        let entry = fuse::EntryOut::read_from_bytes(&reply).unwrap();
        assert_eq!(entry.nodeid, id);
        assert_eq!(entry.attr.nlink, 2);
        // Through the other name.
        test.unlink(ROOT_ID, "a").unwrap();
        assert_eq!(test.lookup(ROOT_ID, "b").unwrap().nodeid, id);
        assert_eq!(test.getattr(id, None).unwrap().nlink, 1);

        // Replacing one name (like `sed -i`) keeps the other one.
        let mut args = fuse::LinkIn { oldnodeid: id }.as_bytes().to_vec();
        args.extend(name("c"));
        test.call(fuse::LINK, ROOT_ID, &args, &[]).unwrap();
        let (new, _) = test.create(ROOT_ID, "tmp", 0o644).unwrap();
        test.rename((ROOT_ID, "tmp"), (ROOT_ID, "c"), 0).unwrap();
        assert_eq!(test.getattr(id, None).unwrap().nlink, 1);
        assert_eq!(test.lookup(ROOT_ID, "c").unwrap().nodeid, new);
        // A rename between two names of a file does nothing.
        let mut args = fuse::LinkIn { oldnodeid: id }.as_bytes().to_vec();
        args.extend(name("d"));
        test.call(fuse::LINK, ROOT_ID, &args, &[]).unwrap();
        test.rename((ROOT_ID, "b"), (ROOT_ID, "d"), 0).unwrap();
        assert_eq!(test.lookup(ROOT_ID, "b").unwrap().nodeid, id);
        assert_eq!(test.lookup(ROOT_ID, "d").unwrap().nodeid, id);
    }

    #[test]
    fn renames_move_inodes() {
        let mut test = Test::new();
        let dir = test.mkdir(ROOT_ID, "dir");
        let (file, _) = test.create(dir, "file", 0o644).unwrap();
        test.rename((ROOT_ID, "dir"), (ROOT_ID, "moved"), 0)
            .unwrap();
        // The file is in the moved directory.
        test.getattr(file, None).unwrap();
        assert!(test.dir.join("moved/file").exists());
        assert_eq!(test.lookup(dir, "file").unwrap().nodeid, file);

        // A rename over a file replaces it; the replaced inode has no name anymore.
        let (other, _) = test.create(ROOT_ID, "other", 0o644).unwrap();
        test.rename((dir, "file"), (ROOT_ID, "other"), 0).unwrap();
        // (Still open.)
        assert_eq!(test.getattr(other, None).unwrap().nlink, 0);
        assert_eq!(test.lookup(ROOT_ID, "other").unwrap().nodeid, file);

        let (third, _) = test.create(ROOT_ID, "third", 0o644).unwrap();
        assert_eq!(
            test.rename(
                (ROOT_ID, "other"),
                (ROOT_ID, "third"),
                fuse::RENAME_NOREPLACE
            ),
            Err(libc::EEXIST)
        );
        test.rename(
            (ROOT_ID, "other"),
            (ROOT_ID, "third"),
            fuse::RENAME_EXCHANGE,
        )
        .unwrap();
        assert_eq!(test.lookup(ROOT_ID, "other").unwrap().nodeid, third);
        assert_eq!(test.lookup(ROOT_ID, "third").unwrap().nodeid, file);
        // No whiteouts.
        assert_eq!(
            test.rename((ROOT_ID, "other"), (ROOT_ID, "third"), 4),
            Err(libc::EINVAL)
        );
        test.rename(
            (ROOT_ID, "other"),
            (ROOT_ID, "fourth"),
            fuse::RENAME_NOREPLACE,
        )
        .unwrap();
    }

    #[test]
    fn host_changes_keep_inodes() {
        let mut test = Test::new();
        let dir = test.mkdir(ROOT_ID, "dir");
        let (file, fh) = test.create(dir, "file", 0o644).unwrap();
        test.write(fh, 0, b"data").unwrap();
        // The host moves the directory and the file: the guest's inodes stay.
        fs::rename(test.dir.join("dir"), test.dir.join("moved")).unwrap();
        fs::rename(test.dir.join("moved/file"), test.dir.join("renamed")).unwrap();
        assert_eq!(test.getattr(file, None).unwrap().size, 4);
        assert_eq!(test.lookup(ROOT_ID, "renamed").unwrap().nodeid, file);
        test.create(dir, "new", 0o644).unwrap();
        assert!(test.dir.join("moved/new").exists());
        // Also when the host unlinks the file.
        fs::remove_file(test.dir.join("renamed")).unwrap();
        assert_eq!(test.getattr(file, None).unwrap().nlink, 0);
        assert_eq!(test.read(fuse::READ, file, fh, 0, 100).unwrap(), b"data");
        let open = fuse::OpenIn {
            flags: 0,
            open_flags: 0,
        };
        let reply = test.call(fuse::OPEN, file, open.as_bytes(), &[]).unwrap();
        let fh = fuse::OpenOut::read_from_bytes(&reply).unwrap().fh;
        assert_eq!(test.read(fuse::READ, file, fh, 0, 100).unwrap(), b"data");
    }

    #[test]
    fn symlinks_are_not_followed() {
        let mut test = Test::new();
        let mut args = name("link");
        args.extend(name("/"));
        let reply = test.call(fuse::SYMLINK, ROOT_ID, &args, &[]).unwrap();
        let link = fuse::EntryOut::read_from_bytes(&reply).unwrap().nodeid;
        assert_eq!(test.call(fuse::READLINK, link, &[], &[]).unwrap(), b"/");
        // Not into the host's root directory.
        assert_eq!(test.lookup(link, "etc").map(drop), Err(libc::ENOTDIR));
        let open = fuse::OpenIn {
            flags: 0,
            open_flags: 0,
        };
        assert_eq!(
            test.call(fuse::OPEN, link, open.as_bytes(), &[]),
            Err(libc::EINVAL)
        );
        assert_eq!(
            test.call(fuse::OPENDIR, link, &[0; 8], &[]),
            Err(libc::ENOTDIR)
        );
        let setattr = fuse::SetattrIn {
            valid: fuse::FATTR_MODE,
            mode: 0o777,
            ..Default::default()
        };
        assert_eq!(
            test.call(fuse::SETATTR, link, setattr.as_bytes(), &[]),
            Err(libc::EOPNOTSUPP)
        );
        // But it has times of its own (`cp -a`, `rsync -a`).
        let setattr = fuse::SetattrIn {
            valid: fuse::FATTR_MTIME,
            mtime: 1_000_000_000,
            ..Default::default()
        };
        let reply = test
            .call(fuse::SETATTR, link, setattr.as_bytes(), &[])
            .unwrap();
        let attr = fuse::AttrOut::read_from_bytes(&reply).unwrap().attr;
        assert_eq!(attr.mtime, 1_000_000_000);
        let lstat = fs::symlink_metadata(test.dir.join("link")).unwrap();
        assert_eq!(lstat.mtime(), 1_000_000_000);
        // A symlink of the host neither.
        std::os::unix::fs::symlink("/", test.dir.join("dir")).unwrap();
        assert_eq!(
            test.lookup(ROOT_ID, "dir").unwrap().attr.mode & S_IFMT,
            S_IFLNK
        );
        assert_eq!(test.create(ROOT_ID, "dir", 0o644), Err(libc::EINVAL));
        assert_eq!(
            test.call(fuse::READLINK, ROOT_ID, &[], &[]),
            Err(libc::EINVAL)
        );
        // Bad names.
        for bad in ["..", ".", "a/b"] {
            assert_eq!(test.lookup(ROOT_ID, bad).map(drop), Err(libc::EINVAL));
        }
    }

    #[test]
    fn read_directories() {
        let mut test = Test::new();
        test.mkdir(ROOT_ID, "dir");
        test.create(ROOT_ID, "file", 0o644).unwrap();
        let reply = test.call(fuse::OPENDIR, ROOT_ID, &[0; 8], &[]).unwrap();
        let fh = fuse::OpenOut::read_from_bytes(&reply).unwrap().fh;

        let reply = test.read(fuse::READDIRPLUS, ROOT_ID, fh, 0, 4096).unwrap();
        let mut names = Test::names(&reply, true);
        names.sort();
        assert_eq!(names[0], (".".to_string(), 0));
        assert_eq!(names[1], ("..".to_string(), 0));
        assert_eq!(names[2].0, "dir");
        assert_eq!(names[2].1, test.lookup(ROOT_ID, "dir").unwrap().nodeid);
        assert_eq!(names[3].0, "file");
        assert_eq!(names.len(), 4);

        // A small buffer gets the first entries; the offset continues after them.
        let reply = test.read(fuse::READDIR, ROOT_ID, fh, 0, 64).unwrap();
        let first = Test::names(&reply, false);
        assert_eq!(first.len(), 2);
        test.create(ROOT_ID, "new", 0o644).unwrap();
        let reply = test.read(fuse::READDIR, ROOT_ID, fh, 2, 4096).unwrap();
        assert_eq!(first.len() + Test::names(&reply, false).len(), 4);
        // Reading from the start again sees the new file.
        let reply = test.read(fuse::READDIR, ROOT_ID, fh, 0, 4096).unwrap();
        assert_eq!(Test::names(&reply, false).len(), 5);
        test.call(fuse::RELEASEDIR, ROOT_ID, fh.as_bytes(), &[])
            .unwrap();
    }

    #[test]
    fn special_files() {
        let mut test = Test::new();
        let mknod = |test: &mut Test, child: &str, mode: u32| {
            let mknod = fuse::MknodIn {
                mode,
                ..Default::default()
            };
            let mut args = mknod.as_bytes().to_vec();
            args.extend(name(child));
            let reply = test.call(fuse::MKNOD, ROOT_ID, &args, &[])?;
            Ok(fuse::EntryOut::read_from_bytes(&reply).unwrap().nodeid)
        };
        #[allow(clippy::unnecessary_cast)]
        let fifo = mknod(&mut test, "fifo", libc::S_IFIFO as u32 | 0o644).unwrap();
        assert_eq!(mknod(&mut test, "null", S_IFCHR | 0o666), Err(libc::EPERM));
        assert_eq!(mknod(&mut test, "disk", S_IFBLK | 0o666), Err(libc::EPERM));
        let regular = mknod(&mut test, "regular", S_IFREG | 0o640).unwrap();
        assert_eq!(test.getattr(regular, None).unwrap().mode, S_IFREG | 0o640);
        // We do not open a FIFO (which would wait for a writer).
        let open = fuse::OpenIn {
            flags: 0,
            open_flags: 0,
        };
        assert_eq!(
            test.call(fuse::OPEN, fifo, open.as_bytes(), &[]),
            Err(libc::EINVAL)
        );
        assert_eq!(test.create(ROOT_ID, "fifo", 0o644), Err(libc::EINVAL));
        let setattr = fuse::SetattrIn {
            valid: fuse::FATTR_SIZE,
            ..Default::default()
        };
        assert_eq!(
            test.call(fuse::SETATTR, fifo, setattr.as_bytes(), &[]),
            Err(libc::EINVAL)
        );
        test.mkdir(ROOT_ID, "dir");
        assert_eq!(test.create(ROOT_ID, "dir", 0o644), Err(libc::EISDIR));
    }

    #[test]
    fn forget_drops_inodes() {
        let mut test = Test::new();
        let dir = test.mkdir(ROOT_ID, "dir");
        let (file, _) = test.create(dir, "file", 0o644).unwrap();
        test.lookup(dir, "file").unwrap();
        assert_eq!(test.inodes(), 3);
        // The file does not need its directory.
        test.forget(dir, 1);
        assert_eq!(test.inodes(), 2);
        assert_eq!(test.getattr(dir, None), Err(libc::ESTALE));
        test.forget(file, 1);
        assert_eq!(test.inodes(), 2);
        test.getattr(file, None).unwrap();
        test.forget(file, 1);
        assert_eq!(test.inodes(), 1);
        assert_eq!(test.getattr(file, None), Err(libc::ESTALE));
        // Looking the file up again makes a new inode.
        let dir = test.lookup(ROOT_ID, "dir").unwrap().nodeid;
        assert_ne!(test.lookup(dir, "file").unwrap().nodeid, file);
    }

    #[test]
    fn submounts() {
        // A directory of the host's / that is another file system (e.g., devtmpfs).
        let root = fs::metadata("/").unwrap().dev();
        let mount = ["dev", "run", "tmp", "boot", "home"]
            .into_iter()
            .find(|name| fs::metadata(Path::new("/").join(name)).is_ok_and(|m| m.dev() != root))
            .expect("a mount point in /");
        let mut test = Test::with_root("/".into(), false, fuse::SUBMOUNTS);
        let entry = test.lookup(ROOT_ID, mount).unwrap();
        assert_eq!(entry.attr.flags, fuse::ATTR_SUBMOUNT);
        let metadata = fs::metadata(Path::new("/").join(mount)).unwrap();
        assert_eq!(entry.attr.ino, metadata.ino());
        assert_eq!(test.getattr(entry.nodeid, None).unwrap().flags, 0);
        // Below it, only directories on yet another file system are submounts.
        let reply = test
            .call(fuse::OPENDIR, entry.nodeid, &[0; 8], &[])
            .unwrap();
        let fh = fuse::OpenOut::read_from_bytes(&reply).unwrap().fh;
        let reply = test
            .read(fuse::READDIRPLUS, entry.nodeid, fh, 0, REPLY_SIZE as u32)
            .unwrap();
        let dir = Path::new("/").join(mount);
        let mut rest = &reply[..];
        while !rest.is_empty() {
            let (entry, tail) = fuse::EntryOut::read_from_prefix(rest).unwrap();
            let (dirent, tail) = fuse::Dirent::read_from_prefix(tail).unwrap();
            let name = std::ffi::OsStr::from_bytes(&tail[..dirent.namelen as usize]);
            // (Entries of /dev may come and go meanwhile.)
            if entry.nodeid != 0
                && let Ok(child) = fs::symlink_metadata(dir.join(name))
            {
                let submount = child.is_dir() && child.dev() != metadata.dev();
                assert_eq!(
                    entry.attr.flags == fuse::ATTR_SUBMOUNT,
                    submount,
                    "{name:?}"
                );
            }
            rest = &tail[(dirent.namelen as usize).next_multiple_of(8)..];
        }

        // Not for drivers without submounts.
        let mut test = Test::with_root("/".into(), false, 0);
        assert_eq!(test.lookup(ROOT_ID, mount).unwrap().attr.flags, 0);
    }

    #[test]
    fn procfs_is_out_of_reach() {
        let mut test = Test::with_root("/".into(), false, fuse::SUBMOUNTS);
        // (Our /proc/self/mem, for example.)
        assert_eq!(test.lookup(ROOT_ID, "proc").map(drop), Err(libc::EPERM));
        assert!(Fs::new(Path::new("/proc")).is_err());
        // Also from READDIRPLUS (no entry), where the guest's listing continues.
        let reply = test.call(fuse::OPENDIR, ROOT_ID, &[0; 8], &[]).unwrap();
        let fh = fuse::OpenOut::read_from_bytes(&reply).unwrap().fh;
        let reply = test
            .read(fuse::READDIRPLUS, ROOT_ID, fh, 0, REPLY_SIZE as u32)
            .unwrap();
        let names = Test::names(&reply, true);
        assert!(names.contains(&("proc".to_string(), 0)));
        assert!(names.iter().any(|(name, id)| name != "proc" && *id != 0));
    }

    #[test]
    fn inodes_are_limited() {
        let mut test = Test::new();
        test.fs.max_inodes = 2;
        let (file, _) = test.create(ROOT_ID, "file", 0o644).unwrap();
        // A file that we know is fine; a new one is not (and does not stay behind).
        assert_eq!(test.lookup(ROOT_ID, "file").unwrap().nodeid, file);
        assert_eq!(test.create(ROOT_ID, "other", 0o644), Err(libc::ENFILE));
        assert!(!test.dir.join("other").exists());
        test.forget(file, 2);
        test.create(ROOT_ID, "other", 0o644).unwrap();
    }

    #[test]
    fn remounts_do_not_reuse_ids() {
        let mut test = Test::new();
        let (file, fh) = test.create(ROOT_ID, "file", 0o644).unwrap();
        let init = fuse::InitIn {
            major: fuse::KERNEL_VERSION,
            minor: fuse::KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
        };
        test.call(fuse::INIT, 0, init.as_bytes(), &[]).unwrap();
        assert_eq!(test.inodes(), 1);
        // A late request of the old mount cannot hit the new mount's objects.
        let (again, again_fh) = test.create(ROOT_ID, "file", 0o644).unwrap();
        assert!(again > file);
        assert!(again_fh > fh);
    }

    #[test]
    fn access() {
        let mut test = Test::new();
        let (file, _) = test.create(ROOT_ID, "file", 0o400).unwrap();
        let access = |test: &mut Test, id, mask| {
            let access = fuse::AccessIn { mask, padding: 0 };
            test.call(fuse::ACCESS, id, access.as_bytes(), &[])
                .map(drop)
        };
        access(&mut test, file, libc::R_OK as u32).unwrap();
        access(
            &mut test,
            ROOT_ID,
            (libc::R_OK | libc::W_OK | libc::X_OK) as u32,
        )
        .unwrap();
        // SAFETY: geteuid() has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(
                access(&mut test, file, libc::W_OK as u32),
                Err(libc::EACCES)
            );
        }
    }

    #[test]
    fn resets_start_over() {
        let mut test = Test::new();
        let (file, _) = test.create(ROOT_ID, "file", 0o644).unwrap();
        test.generation = 1;
        // A request of the new driver forgets everything.
        assert_eq!(test.getattr(file, None), Err(libc::ESTALE));
        // Those from before the reset are stale.
        let header = fuse::InHeader {
            opcode: fuse::LOOKUP,
            nodeid: ROOT_ID,
            ..Default::default()
        };
        let reply = [Buffer {
            address: REPLY,
            length: REPLY_SIZE,
        }];
        let args = name("file");
        assert_eq!(
            test.fs
                .execute(0, &header, &args, &[], &reply, &test.memory),
            None
        );
        assert_eq!(test.inodes(), 1);
    }

    #[test]
    fn requests_through_the_queue() {
        let test = Test::new();
        let (mut device, backend) = VirtioFs::new("tag", &test.dir).unwrap();
        let memory = memory();
        let mut queues = [queue(), queue()];
        let mut config = [0u8; 40];
        device.read_config(0, &mut config);
        assert!(config.starts_with(b"tag\0"));
        assert_eq!(config[36..], [1, 0, 0, 0]);

        // A LOOKUP of a missing file: the header and the name in one buffer, the reply
        // header in another.
        let header = fuse::InHeader {
            len: (IN_HEADER + 8) as u32,
            opcode: fuse::LOOKUP,
            unique: 42,
            nodeid: ROOT_ID,
            ..Default::default()
        };
        let mut request = header.as_bytes().to_vec();
        request.extend(name("missing"));
        memory.write(0x4000, &request).unwrap();
        descriptor(&memory, 0, 0x4000, request.len() as u32, DESC_NEXT, 1);
        descriptor(&memory, 1, 0x5000, 256, DESC_WRITE, 0);
        offer(&memory, 0, 0);
        assert!(!device.process(1, &mut queues, &memory));
        assert_eq!(device.in_flight.count(), 1);

        let request = backend.requests.try_recv().unwrap();
        let completion = request
            .execute(&backend.fs, &memory, &backend.generation)
            .unwrap();
        assert!(device.complete(completion, &mut queues, &memory));
        assert_eq!(device.in_flight.count(), 0);
        assert_eq!(memory.read_volatile::<u16>(USED + 2).unwrap(), 1);
        assert_eq!(
            memory.read_volatile::<u32>(USED + 8).unwrap(),
            OUT_HEADER as u32
        );
        let out: fuse::OutHeader = memory.read_obj(0x5000).unwrap();
        assert_eq!((out.error, out.unique), (-libc::ENOENT, 42));

        // A WRITE without its data gets an error right away.
        let header = fuse::InHeader {
            opcode: fuse::WRITE,
            unique: 43,
            ..Default::default()
        };
        let write = fuse::WriteIn {
            size: 100,
            ..Default::default()
        };
        let mut request = header.as_bytes().to_vec();
        request.extend(write.as_bytes());
        memory.write(0x4000, &request).unwrap();
        descriptor(&memory, 0, 0x4000, request.len() as u32, DESC_NEXT, 1);
        offer(&memory, 1, 0);
        assert!(device.process(1, &mut queues, &memory));
        assert!(backend.requests.is_empty());
        assert_eq!(
            memory.read_volatile::<u32>(USED + 16).unwrap(),
            OUT_HEADER as u32
        );
        let out: fuse::OutHeader = memory.read_obj(0x5000).unwrap();
        assert_eq!((out.error, out.unique), (-libc::EINVAL, 43));
    }
}
