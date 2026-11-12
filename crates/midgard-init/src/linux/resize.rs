//! Growing the root file system to the size of its disk: the disk can be larger than
//! the template's file system (midgard grows disks, the file systems in them stay). ext4
//! grows while it is mounted, so the kernel does the work.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use anyhow::{Context, Result, bail};

use super::log;

// The libc crate does not define these.
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
const EXT4_IOC_RESIZE_FS: libc::c_ulong = 0x4008_6610;

/// Where the ext4 superblock is on the disk.
const SUPERBLOCK_OFFSET: u64 = 1024;
const EXT4_MAGIC: u16 = 0xEF53;
/// The file system has 64-bit block numbers (and a high half of the block count).
const INCOMPAT_64BIT: u32 = 0x80;

/// What we need from an ext4 superblock.
#[derive(Debug, PartialEq, Eq)]
struct Superblock {
    blocks: u64,
    block_size: u64,
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Parses an ext2/3/4 superblock (1024 bytes); None if it is not one.
fn parse_superblock(bytes: &[u8; 1024]) -> Option<Superblock> {
    if u16::from_le_bytes([bytes[0x38], bytes[0x39]]) != EXT4_MAGIC {
        return None;
    }
    let log_block_size = read_u32(bytes, 0x18);
    if log_block_size > 6 {
        return None;
    }
    let mut blocks = read_u32(bytes, 0x04) as u64;
    if read_u32(bytes, 0x60) & INCOMPAT_64BIT != 0 {
        blocks |= (read_u32(bytes, 0x150) as u64) << 32;
    }
    Some(Superblock {
        blocks,
        block_size: 1024 << log_block_size,
    })
}

/// Grows the ext4 file system on `device` (mounted at `mountpoint`) to the size of the
/// device. Problems are only reported: the guest still boots with the smaller one.
pub fn grow_root(device: &str, mountpoint: &str) {
    if let Err(error) = try_grow(device, mountpoint) {
        log!("cannot grow the root file system: {error:#}");
    }
}

fn try_grow(device: &str, mountpoint: &str) -> Result<()> {
    let disk = File::open(device).with_context(|| format!("opening {device}"))?;
    let mut bytes = [0u8; 1024];
    disk.read_exact_at(&mut bytes, SUPERBLOCK_OFFSET)
        .context("reading the superblock")?;
    let Some(superblock) = parse_superblock(&bytes) else {
        return Ok(());
    };
    let mut size = 0u64;
    // SAFETY: BLKGETSIZE64 writes a u64.
    if unsafe { libc::ioctl(disk.as_raw_fd(), BLKGETSIZE64 as libc::Ioctl, &mut size) } != 0 {
        return Err(std::io::Error::last_os_error()).context("BLKGETSIZE64");
    }
    let blocks = size / superblock.block_size;
    if blocks <= superblock.blocks {
        return Ok(());
    }
    let root = File::open(mountpoint).with_context(|| format!("opening {mountpoint}"))?;
    // SAFETY: EXT4_IOC_RESIZE_FS reads a u64 (the new block count).
    if unsafe { libc::ioctl(root.as_raw_fd(), EXT4_IOC_RESIZE_FS as libc::Ioctl, &blocks) } != 0 {
        let error = std::io::Error::last_os_error();
        bail!(
            "EXT4_IOC_RESIZE_FS from {} to {blocks} blocks: {error}",
            superblock.blocks
        );
    }
    log!(
        "grew the root file system from {} to {} MiB",
        (superblock.blocks * superblock.block_size) >> 20,
        (blocks * superblock.block_size) >> 20
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_of_mke2fs() {
        let dir = std::env::temp_dir().join(format!("midgard-init-sb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fs.ext4");
        File::create(&path).unwrap().set_len(96 << 20).unwrap();
        let Ok(status) = std::process::Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext4", "-b", "4096"])
            .arg(&path)
            .status()
        else {
            // No e2fsprogs.
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        };
        assert!(status.success());
        let mut bytes = [0u8; 1024];
        File::open(&path)
            .unwrap()
            .read_exact_at(&mut bytes, SUPERBLOCK_OFFSET)
            .unwrap();
        assert_eq!(
            parse_superblock(&bytes),
            Some(Superblock {
                blocks: (96 << 20) / 4096,
                block_size: 4096,
            })
        );
        bytes[0x38] = 0;
        assert_eq!(parse_superblock(&bytes), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
