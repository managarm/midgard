//! The initramfs that runs midgard-init: built in memory as a "newc" cpio archive.

/// Builds an archive with /init (the agent), /dev/console and the directories that the
/// agent mounts on.
pub(crate) fn build(init: &[u8]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut inode = 1;
    let mut add = |name: &str, mode: u32, rdev: (u32, u32), data: &[u8]| {
        let nlink = if mode & 0o170000 == 0o040000 { 2 } else { 1 };
        let header = format!(
            "070701{inode:08X}{mode:08X}{uid:08X}{gid:08X}{nlink:08X}{mtime:08X}{size:08X}\
             {devmajor:08X}{devminor:08X}{rdevmajor:08X}{rdevminor:08X}{namesize:08X}{check:08X}",
            uid = 0,
            gid = 0,
            mtime = 0,
            size = data.len(),
            devmajor = 0,
            devminor = 0,
            rdevmajor = rdev.0,
            rdevminor = rdev.1,
            namesize = name.len() + 1,
            check = 0,
        );
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(name.as_bytes());
        archive.push(0);
        // The name (after the 110-byte header) and the data are padded to 4 bytes.
        archive.resize(archive.len().next_multiple_of(4), 0);
        archive.extend_from_slice(data);
        archive.resize(archive.len().next_multiple_of(4), 0);
        inode += 1;
    };
    for dir in ["dev", "proc", "sys", "newroot"] {
        add(dir, 0o040755, (0, 0), &[]);
    }
    // The kernel opens /dev/console for init before init runs.
    add("dev/console", 0o020600, (5, 1), &[]);
    add("init", 0o100755, (0, 0), init);
    add("TRAILER!!!", 0, (0, 0), &[]);
    archive
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let archive = build(b"#!binary");
        assert!(archive.starts_with(b"070701"));
        assert_eq!(archive.len() % 4, 0);
        // Every entry starts at a 4-byte boundary with the magic.
        let text = String::from_utf8_lossy(&archive);
        assert_eq!(text.matches("070701").count(), 7);
        assert!(text.contains("init\0"));
        assert!(text.contains("TRAILER!!!\0"));
        assert!(archive.windows(8).any(|w| w == b"#!binary"));
    }
}
