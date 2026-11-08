//! The parts of the FUSE protocol (`linux/fuse.h`, version 7.32) that virtio-fs uses.
//!
//! The structures are in the guest's byte order, which is ours (x86). Request
//! structures may be followed by names or data; replies are a [`OutHeader`] and a reply
//! structure (or data).

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub(crate) const KERNEL_VERSION: u32 = 7;
pub(crate) const KERNEL_MINOR_VERSION: u32 = 32;
/// The node ID of the root directory.
pub(crate) const ROOT_ID: u64 = 1;

// Opcodes.
pub(crate) const LOOKUP: u32 = 1;
pub(crate) const FORGET: u32 = 2;
pub(crate) const GETATTR: u32 = 3;
pub(crate) const SETATTR: u32 = 4;
pub(crate) const READLINK: u32 = 5;
pub(crate) const SYMLINK: u32 = 6;
pub(crate) const MKNOD: u32 = 8;
pub(crate) const MKDIR: u32 = 9;
pub(crate) const UNLINK: u32 = 10;
pub(crate) const RMDIR: u32 = 11;
pub(crate) const RENAME: u32 = 12;
pub(crate) const LINK: u32 = 13;
pub(crate) const OPEN: u32 = 14;
pub(crate) const READ: u32 = 15;
pub(crate) const WRITE: u32 = 16;
pub(crate) const STATFS: u32 = 17;
pub(crate) const RELEASE: u32 = 18;
pub(crate) const FSYNC: u32 = 20;
pub(crate) const FLUSH: u32 = 25;
pub(crate) const INIT: u32 = 26;
pub(crate) const OPENDIR: u32 = 27;
pub(crate) const READDIR: u32 = 28;
pub(crate) const RELEASEDIR: u32 = 29;
pub(crate) const FSYNCDIR: u32 = 30;
pub(crate) const ACCESS: u32 = 34;
pub(crate) const CREATE: u32 = 35;
pub(crate) const INTERRUPT: u32 = 36;
pub(crate) const DESTROY: u32 = 38;
pub(crate) const BATCH_FORGET: u32 = 42;
pub(crate) const READDIRPLUS: u32 = 44;
pub(crate) const RENAME2: u32 = 45;

// INIT flags.
pub(crate) const ASYNC_READ: u32 = 1 << 0;
pub(crate) const ATOMIC_O_TRUNC: u32 = 1 << 3;
pub(crate) const BIG_WRITES: u32 = 1 << 5;
pub(crate) const AUTO_INVAL_DATA: u32 = 1 << 12;
pub(crate) const DO_READDIRPLUS: u32 = 1 << 13;
pub(crate) const PARALLEL_DIROPS: u32 = 1 << 18;
pub(crate) const MAX_PAGES: u32 = 1 << 22;
/// The driver mounts directories with [`ATTR_SUBMOUNT`] as file systems of their own.
pub(crate) const SUBMOUNTS: u32 = 1 << 27;

/// [`Attr`] flags: the directory is a submount (another file system of the host).
pub(crate) const ATTR_SUBMOUNT: u32 = 1 << 0;

// SETATTR valid bits.
pub(crate) const FATTR_MODE: u32 = 1 << 0;
pub(crate) const FATTR_UID: u32 = 1 << 1;
pub(crate) const FATTR_GID: u32 = 1 << 2;
pub(crate) const FATTR_SIZE: u32 = 1 << 3;
pub(crate) const FATTR_ATIME: u32 = 1 << 4;
pub(crate) const FATTR_MTIME: u32 = 1 << 5;
pub(crate) const FATTR_FH: u32 = 1 << 6;
pub(crate) const FATTR_ATIME_NOW: u32 = 1 << 7;
pub(crate) const FATTR_MTIME_NOW: u32 = 1 << 8;

/// GETATTR: `fh` is valid (fstat()).
pub(crate) const GETATTR_FH: u32 = 1 << 0;
/// FSYNC: only the data (fdatasync()).
pub(crate) const FSYNC_FDATASYNC: u32 = 1 << 0;
// RENAME2 flags (renameat2()).
pub(crate) const RENAME_NOREPLACE: u32 = 1 << 0;
pub(crate) const RENAME_EXCHANGE: u32 = 1 << 1;

// The guest's open(2) flags (Linux values, which are not necessarily the host's).
pub(crate) const O_ACCMODE: u32 = 0o3;
pub(crate) const O_WRONLY: u32 = 0o1;
pub(crate) const O_RDWR: u32 = 0o2;
pub(crate) const O_EXCL: u32 = 0o200;
pub(crate) const O_TRUNC: u32 = 0o1000;
pub(crate) const O_APPEND: u32 = 0o2000;
pub(crate) const O_DSYNC: u32 = 0o10000;
pub(crate) const O_SYNC: u32 = 0o4010000;

/// Derives the traits that FUSE structures need.
macro_rules! fuse_struct {
    ($(#[$meta:meta])* $name:ident { $($field:ident: $type:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
        )]
        #[repr(C)]
        pub(crate) struct $name {
            $(pub(crate) $field: $type),*
        }
    };
}

fuse_struct!(
    /// struct fuse_in_header.
    InHeader {
        len: u32,
        opcode: u32,
        unique: u64,
        nodeid: u64,
        uid: u32,
        gid: u32,
        pid: u32,
        total_extlen: u16,
        padding: u16,
    }
);

fuse_struct!(
    /// struct fuse_out_header.
    OutHeader {
        len: u32,
        error: i32,
        unique: u64,
    }
);

fuse_struct!(
    /// struct fuse_attr.
    Attr {
        ino: u64,
        size: u64,
        blocks: u64,
        atime: u64,
        mtime: u64,
        ctime: u64,
        atimensec: u32,
        mtimensec: u32,
        ctimensec: u32,
        mode: u32,
        nlink: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        blksize: u32,
        flags: u32,
    }
);

fuse_struct!(
    /// struct fuse_entry_out.
    EntryOut {
        nodeid: u64,
        generation: u64,
        entry_valid: u64,
        attr_valid: u64,
        entry_valid_nsec: u32,
        attr_valid_nsec: u32,
        attr: Attr,
    }
);

fuse_struct!(
    /// struct fuse_attr_out.
    AttrOut {
        attr_valid: u64,
        attr_valid_nsec: u32,
        dummy: u32,
        attr: Attr,
    }
);

fuse_struct!(
    /// struct fuse_forget_in.
    ForgetIn { nlookup: u64 }
);

fuse_struct!(
    /// struct fuse_batch_forget_in, followed by `count` [`ForgetOne`]s.
    BatchForgetIn { count: u32, dummy: u32 }
);

fuse_struct!(
    /// struct fuse_forget_one.
    ForgetOne { nodeid: u64, nlookup: u64 }
);

fuse_struct!(
    /// The start of struct fuse_init_in (newer guests send more).
    InitIn {
        major: u32,
        minor: u32,
        max_readahead: u32,
        flags: u32,
    }
);

fuse_struct!(
    /// struct fuse_init_out.
    InitOut {
        major: u32,
        minor: u32,
        max_readahead: u32,
        flags: u32,
        max_background: u16,
        congestion_threshold: u16,
        max_write: u32,
        time_gran: u32,
        max_pages: u16,
        map_alignment: u16,
        flags2: u32,
        max_stack_depth: u32,
        unused: [u32; 6],
    }
);

fuse_struct!(
    /// struct fuse_setattr_in.
    SetattrIn {
        valid: u32,
        padding: u32,
        fh: u64,
        size: u64,
        lock_owner: u64,
        atime: u64,
        mtime: u64,
        ctime: u64,
        atimensec: u32,
        mtimensec: u32,
        ctimensec: u32,
        mode: u32,
        unused4: u32,
        uid: u32,
        gid: u32,
        unused5: u32,
    }
);

fuse_struct!(
    /// struct fuse_getattr_in.
    GetattrIn {
        getattr_flags: u32,
        dummy: u32,
        fh: u64,
    }
);

fuse_struct!(
    /// struct fuse_mknod_in, followed by the name.
    MknodIn {
        mode: u32,
        rdev: u32,
        umask: u32,
        padding: u32,
    }
);

fuse_struct!(
    /// struct fuse_mkdir_in, followed by the name.
    MkdirIn { mode: u32, umask: u32 }
);

fuse_struct!(
    /// struct fuse_rename_in, followed by the old and the new name.
    RenameIn { newdir: u64 }
);

fuse_struct!(
    /// struct fuse_rename2_in, followed by the old and the new name.
    Rename2In {
        newdir: u64,
        flags: u32,
        padding: u32,
    }
);

fuse_struct!(
    /// struct fuse_link_in, followed by the name.
    LinkIn { oldnodeid: u64 }
);

fuse_struct!(
    /// struct fuse_open_in.
    OpenIn { flags: u32, open_flags: u32 }
);

fuse_struct!(
    /// struct fuse_access_in.
    AccessIn { mask: u32, padding: u32 }
);

fuse_struct!(
    /// struct fuse_create_in, followed by the name.
    CreateIn {
        flags: u32,
        mode: u32,
        umask: u32,
        open_flags: u32,
    }
);

fuse_struct!(
    /// struct fuse_open_out.
    OpenOut {
        fh: u64,
        open_flags: u32,
        padding: u32,
    }
);

fuse_struct!(
    /// struct fuse_read_in (also for READDIR).
    ReadIn {
        fh: u64,
        offset: u64,
        size: u32,
        read_flags: u32,
        lock_owner: u64,
        flags: u32,
        padding: u32,
    }
);

fuse_struct!(
    /// struct fuse_write_in, followed by the data.
    WriteIn {
        fh: u64,
        offset: u64,
        size: u32,
        write_flags: u32,
        lock_owner: u64,
        flags: u32,
        padding: u32,
    }
);

fuse_struct!(
    /// struct fuse_write_out.
    WriteOut { size: u32, padding: u32 }
);

fuse_struct!(
    /// struct fuse_statfs_out (struct fuse_kstatfs).
    StatfsOut {
        blocks: u64,
        bfree: u64,
        bavail: u64,
        files: u64,
        ffree: u64,
        bsize: u32,
        namelen: u32,
        frsize: u32,
        padding: u32,
        spare: [u32; 6],
    }
);

fuse_struct!(
    /// The start of struct fuse_release_in: the handle.
    HandleIn { fh: u64 }
);

fuse_struct!(
    /// struct fuse_fsync_in.
    FsyncIn {
        fh: u64,
        fsync_flags: u32,
        padding: u32,
    }
);

fuse_struct!(
    /// struct fuse_dirent, followed by the name (padded to 8 bytes).
    Dirent {
        ino: u64,
        off: u64,
        namelen: u32,
        type_: u32,
    }
);

const _: () = {
    assert!(size_of::<InHeader>() == 40);
    assert!(size_of::<OutHeader>() == 16);
    assert!(size_of::<Attr>() == 88);
    assert!(size_of::<EntryOut>() == 128);
    assert!(size_of::<AttrOut>() == 104);
    assert!(size_of::<InitOut>() == 64);
    assert!(size_of::<GetattrIn>() == 16);
    assert!(size_of::<SetattrIn>() == 88);
    assert!(size_of::<ReadIn>() == 40);
    assert!(size_of::<WriteIn>() == 40);
    assert!(size_of::<StatfsOut>() == 80);
    assert!(size_of::<Dirent>() == 24);
};
