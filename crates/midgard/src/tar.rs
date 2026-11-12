//! Unpacks the tar files that templates are distributed as. guest/build.py writes them in
//! the pax format, but people make them with whatever they have, so we read what tar
//! programs write (ustar, pax and GNU archives, also compressed with zstd; see the [`tar`]
//! crate). We unpack only regular files and directories (and hard links as copies of the
//! files that they link to) and skip the rest (e.g., symbolic links).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use midgard_protocol::size::format_size;
use tar::{Archive, EntryType};

use crate::{log, zstd};

/// The most that an archive may unpack to: sparse files and hard links can unpack to much
/// more than the archive's size.
const MAX_UNPACKED: u64 = 64 << 30;

/// A member of an archive that [`extract`] skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Its path in the archive.
    pub path: PathBuf,
    /// What it is, for people (e.g., "a symbolic link").
    pub what: &'static str,
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}, which is {}", self.path, self.what)
    }
}

/// Notes whether the input ended, which an archive does only after its end-of-archive
/// blocks (which the [`tar`] crate does not read) unless it is truncated.
struct Ends<R> {
    input: R,
    ended: bool,
}

impl<R: Read> Read for Ends<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.input.read(buffer)?;
        if count == 0 && !buffer.is_empty() {
            self.ended = true;
        }
        Ok(count)
    }
}

/// Where the archive's member `path` goes in `to`: None for the archive's top directory,
/// and for paths that leave it. Like tar programs, we ignore `.` and a leading `/`.
fn target(path: &Path, to: &Path) -> Option<PathBuf> {
    let mut target = to.to_path_buf();
    for component in path.components() {
        match component {
            Component::Normal(part) => target.push(part),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (target != to).then_some(target)
}

/// Writes what `data` reads to the file `target` (replacing it, as tar programs do with
/// later members of the same name) and syncs it; fails if that is more than `budget` (what
/// is left of `limit`).
fn write_file(target: &Path, data: &mut impl Read, budget: u64, limit: u64) -> Result<u64> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create {parent:?}"))?;
    }
    let mut file = File::create(target).with_context(|| format!("failed to create {target:?}"))?;
    let copied = io::copy(&mut data.take(budget.saturating_add(1)), &mut file)
        .with_context(|| format!("failed to write {target:?}"))?;
    if copied > budget {
        bail!("it unpacks to more than {}", format_size(limit));
    }
    file.sync_all()
        .with_context(|| format!("failed to write {target:?}"))?;
    Ok(copied)
}

fn skip(skipped: &mut Vec<Skipped>, path: &Path, what: &'static str) {
    log!("skipping {path:?}, which is {what}");
    skipped.push(Skipped {
        path: path.to_path_buf(),
        what,
    });
}

/// Unpacks the regular files, directories and hard links of `archive` into `to`, at most
/// `limit` bytes; returns what it skipped.
fn unpack(archive: impl Read, to: &Path, limit: u64) -> Result<Vec<Skipped>> {
    let mut archive = Archive::new(Ends {
        input: archive,
        ended: false,
    });
    let mut skipped = Vec::new();
    let mut unpacked = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // (The tar crate would unpack the map of such a file as its data.)
        if let Some(extensions) = entry.pax_extensions()? {
            for extension in extensions {
                if extension?
                    .key()
                    .is_ok_and(|key| key.starts_with("GNU.sparse."))
                {
                    bail!(
                        "{path:?} is a sparse file in the pax format, which midgard cannot \
                         unpack (as GNU tar writes with --sparse --format=posix, and bsdtar \
                         for files with holes; GNU tar's default format works)"
                    );
                }
            }
        }
        let kind = entry.header().entry_type();
        let what = match kind {
            EntryType::Regular
            | EntryType::Continuous
            | EntryType::GNUSparse
            | EntryType::Directory
            | EntryType::Link => None,
            // (What it says applies to the members after it, which the tar crate reads.)
            EntryType::XGlobalHeader => continue,
            EntryType::Symlink => Some("a symbolic link"),
            EntryType::Char | EntryType::Block => Some("a device"),
            EntryType::Fifo => Some("a FIFO"),
            _ => Some("neither a file nor a directory"),
        };
        if let Some(what) = what {
            skip(&mut skipped, &path, what);
            continue;
        }
        let Some(target) = target(&path, to) else {
            if path.components().any(|c| c == Component::ParentDir) {
                skip(&mut skipped, &path, "outside the archive's directory");
            }
            continue;
        };
        let budget = limit - unpacked;
        match kind {
            EntryType::Directory => std::fs::create_dir_all(&target)
                .with_context(|| format!("failed to create {target:?}"))?,
            EntryType::Link => {
                // A copy of a regular file that we unpacked (we make nothing else).
                let source = entry
                    .link_name()?
                    .and_then(|link| self::target(&link, to))
                    .filter(|source| *source != target)
                    .filter(|source| source.symlink_metadata().is_ok_and(|m| m.is_file()));
                let Some(source) = source else {
                    skip(
                        &mut skipped,
                        &path,
                        "a hard link to no file before it in the archive",
                    );
                    continue;
                };
                let mut data =
                    File::open(&source).with_context(|| format!("failed to open {source:?}"))?;
                unpacked += write_file(&target, &mut data, budget, limit)?;
            }
            _ => {
                unpacked += write_file(&target, &mut entry, budget, limit)
                    .with_context(|| format!("failed to unpack {path:?}"))?;
            }
        }
    }
    if archive.into_inner().ended {
        bail!("the archive is truncated (it ends before its end-of-archive blocks)");
    }
    Ok(skipped)
}

fn extract_limited(reader: impl Read, to: &Path, limit: u64) -> Result<Vec<Skipped>> {
    std::fs::create_dir(to).with_context(|| format!("failed to create {to:?}"))?;
    let mut input = BufReader::new(reader);
    if zstd::is_compressed(input.fill_buf()?) {
        unpack(zstd::Frames::new(input), to, limit)
    } else {
        unpack(input, to, limit)
    }
}

/// Unpacks the regular files and directories of the archive (which may be compressed with
/// zstd) into the new directory `to`; returns the members that it skipped.
pub fn extract(reader: impl Read, to: &Path) -> Result<Vec<Skipped>> {
    extract_limited(reader, to, MAX_UNPACKED)
}

#[cfg(test)]
pub mod tests {
    use ruzstd::encoding::{CompressionLevel, compress_to_vec};

    use super::*;

    const BLOCK: usize = 512;
    const TEMPLATE: &[u8] = b"[guest]\n";

    /// Fills in a header's checksum.
    fn seal(header: &mut [u8; BLOCK]) {
        header[148..156].fill(b' ');
        let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    }

    fn pad(bytes: &mut Vec<u8>) {
        bytes.resize(bytes.len().next_multiple_of(BLOCK), 0);
    }

    /// A member (`kind`: its type flag; `link`: what it links to) in the ustar format.
    pub fn member(name: &str, kind: u8, link: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[156] = kind;
        header[157..157 + link.len()].copy_from_slice(link.as_bytes());
        header[257..265].copy_from_slice(b"ustar\x0000");
        seal(&mut header);
        let mut member = header.to_vec();
        member.extend_from_slice(data);
        pad(&mut member);
        member
    }

    /// A pax extended header with `records`, for the member after it.
    fn pax(records: &[(&str, &str)]) -> Vec<u8> {
        let mut data = String::new();
        for (key, value) in records {
            // The length counts its own digits.
            let rest = key.len() + value.len() + 3;
            let mut length = rest + 1;
            while length != rest + length.to_string().len() {
                length += 1;
            }
            data += &format!("{length} {key}={value}\n");
        }
        member("PaxHeaders/member", b'x', "", data.as_bytes())
    }

    /// A sparse file of `size` bytes in the GNU format with `chunks` (where they are and what
    /// they have).
    fn gnu_sparse(name: &str, size: u64, chunks: &[(u64, &[u8])]) -> Vec<u8> {
        let data: Vec<u8> = chunks.iter().flat_map(|(_, data)| data.to_vec()).collect();
        let mut header = [0u8; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[156] = b'S';
        header[257..265].copy_from_slice(b"ustar  \0");
        for (index, (offset, chunk)) in chunks.iter().enumerate() {
            let at = 386 + index * 24;
            header[at..at + 12].copy_from_slice(format!("{offset:011o}\0").as_bytes());
            header[at + 12..at + 24].copy_from_slice(format!("{:011o}\0", chunk.len()).as_bytes());
        }
        header[483..495].copy_from_slice(format!("{size:011o}\0").as_bytes());
        seal(&mut header);
        let mut member = header.to_vec();
        member.extend(data);
        pad(&mut member);
        member
    }

    /// An archive of `members`, with its end-of-archive blocks.
    pub fn finish(members: &[Vec<u8>]) -> Vec<u8> {
        let mut archive = members.concat();
        archive.resize(archive.len() + 2 * BLOCK, 0);
        archive
    }

    /// An archive with members (their names, type flags and data).
    pub fn archive(members: &[(&str, u8, &[u8])]) -> Vec<u8> {
        let members: Vec<_> = members
            .iter()
            .map(|(name, kind, data)| member(name, *kind, "", data))
            .collect();
        finish(&members)
    }

    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("midgard-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The names in the directory `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn extracts() {
        let dir = scratch("tar");
        let data = vec![7u8; 1000];
        let archive = archive(&[
            ("alpine", b'5', b""),
            // What Python's tarfile writes in front of each member (e.g., its exact mtime).
            (
                "alpine/PaxHeaders/template.toml",
                b'x',
                b"16 mtime=1731.5\n",
            ),
            ("alpine/template.toml", b'0', TEMPLATE),
            ("ignored", b'x', b"26 path=alpine/disk.qcow2\n"),
            ("alpine/placeholder", b'0', &data),
            ("alpine/sub/", b'5', b""),
            ("alpine/sub/file", 0, b""),
            ("global", b'g', b"15 comment=hey\n"),
        ]);
        let to = dir.join("out");
        assert_eq!(extract(archive.as_slice(), &to).unwrap(), []);
        assert_eq!(
            std::fs::read(to.join("alpine/template.toml")).unwrap(),
            TEMPLATE
        );
        assert_eq!(std::fs::read(to.join("alpine/disk.qcow2")).unwrap(), data);
        assert_eq!(std::fs::read(to.join("alpine/sub/file")).unwrap(), b"");
        assert!(!to.join("alpine/placeholder").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn accepts_what_tar_programs_write() {
        let dir = scratch("tar-programs");
        // `tar -C alpine -cf alpine.tar .`, with a GNU long name and a member without a
        // directory before it.
        let long = format!("./{}/file", "d".repeat(120));
        let mut name = long.clone().into_bytes();
        name.push(0);
        let archive = archive(&[
            ("./", b'5', b""),
            ("./template.toml", b'0', TEMPLATE),
            ("././@LongLink", b'L', &name),
            (&long[..100], b'0', b"long"),
            ("/absolute/file", b'0', b"absolute"),
        ]);
        let to = dir.join("plain");
        extract(archive.as_slice(), &to).unwrap();
        assert_eq!(std::fs::read(to.join("template.toml")).unwrap(), TEMPLATE);
        assert_eq!(std::fs::read(to.join(&long)).unwrap(), b"long");
        assert_eq!(
            std::fs::read(to.join("absolute/file")).unwrap(),
            b"absolute"
        );

        // Compressed with zstd: in two frames, after a skippable frame (as pzstd writes).
        let (first, second) = archive.split_at(1000);
        let mut compressed = 0x184d_2a50u32.to_le_bytes().to_vec();
        compressed.extend(4u32.to_le_bytes());
        compressed.extend(b"skip");
        compressed.extend(compress_to_vec(first, CompressionLevel::Fastest));
        compressed.extend(compress_to_vec(second, CompressionLevel::Fastest));
        let to = dir.join("zstd");
        extract(compressed.as_slice(), &to).unwrap();
        assert_eq!(std::fs::read(to.join("template.toml")).unwrap(), TEMPLATE);
        assert_eq!(std::fs::read(to.join(&long)).unwrap(), b"long");

        // A sparse file in the GNU format (tar --sparse), a hard link to it (as tar programs
        // write files with several links) and a file that a later member replaces.
        let archive = finish(&[
            // (Chunks but the last fill whole blocks.)
            gnu_sparse("a/disk", 3000, &[(1024, &[b'd'; 512]), (2996, b"tail")]),
            member("a/copy", b'1', "./a/disk", b""),
            member("a/file", b'0', "", b"old"),
            member("a/file", b'0', "", b"new"),
        ]);
        let to = dir.join("sparse");
        assert_eq!(extract(archive.as_slice(), &to).unwrap(), []);
        let mut disk = vec![0u8; 3000];
        disk[1024..1536].fill(b'd');
        disk[2996..].copy_from_slice(b"tail");
        assert_eq!(std::fs::read(to.join("a/disk")).unwrap(), disk);
        assert_eq!(std::fs::read(to.join("a/copy")).unwrap(), disk);
        assert_eq!(std::fs::read(to.join("a/file")).unwrap(), b"new");
        // (Which counts its size, not that of its data in the archive.)
        let error = extract_limited(archive.as_slice(), &dir.join("limited"), 5000).unwrap_err();
        assert!(format!("{error:#}").contains("more than"), "{error:#}");

        // An archive without members.
        let to = dir.join("empty");
        assert_eq!(extract(finish(&[]).as_slice(), &to).unwrap(), []);
        assert!(names(&to).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn skips_what_is_no_file() {
        let dir = scratch("tar-skips");
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let outside = outside.to_str().unwrap();
        let archive = finish(&[
            member("alpine/template.toml", b'0', "", TEMPLATE),
            member("alpine/disk.qcow2", b'2', "/dev/sda", b""),
            member("alpine/hardlink", b'1', "/etc/shadow", b""),
            member("alpine/self", b'1', "alpine/self", b""),
            member("alpine/directory", b'1', "alpine", b""),
            member("alpine/later", b'1', "alpine/file", b""),
            member("alpine/fifo", b'6', "", b""),
            member("alpine/device", b'3', "", b""),
            member("alpine/../escape", b'0', "", b"out"),
            member("../escape", b'5', "", b""),
            // Through a symbolic link that we skipped, and with paths from extended headers.
            member("alpine/esc", b'2', outside, b""),
            member("alpine/esc/pwned", b'0', "", b"pwned"),
            pax(&[("path", "alpine/../../pax")]),
            member("alpine/placeholder", b'0', "", b"pax"),
            member("././@LongLink", b'L', "", b"../../long\0"),
            member("alpine/placeholder", b'0', "", b"long"),
            member("alpine/file", b'0', "", b""),
        ]);
        let to = dir.join("out");
        let skipped = extract(archive.as_slice(), &to).unwrap();
        let skipped: Vec<_> = skipped
            .iter()
            .map(|member| (member.path.to_str().unwrap(), member.what))
            .collect();
        let no_file = "a hard link to no file before it in the archive";
        let outside_the_archive = "outside the archive's directory";
        assert_eq!(
            skipped,
            [
                ("alpine/disk.qcow2", "a symbolic link"),
                ("alpine/hardlink", no_file),
                ("alpine/self", no_file),
                ("alpine/directory", no_file),
                ("alpine/later", no_file),
                ("alpine/fifo", "a FIFO"),
                ("alpine/device", "a device"),
                ("alpine/../escape", outside_the_archive),
                ("../escape", outside_the_archive),
                ("alpine/esc", "a symbolic link"),
                ("alpine/../../pax", outside_the_archive),
                ("../../long", outside_the_archive),
            ]
        );
        assert_eq!(names(&to.join("alpine")), ["esc", "file", "template.toml"]);
        assert!(to.join("alpine/esc").symlink_metadata().unwrap().is_dir());
        assert!(names(Path::new(outside)).is_empty());
        assert_eq!(names(&dir), ["out", "outside"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects() {
        let dir = scratch("tar-rejects");
        let rejected = |name: &str, archive: &[u8]| {
            let error = extract(archive, &dir.join(name)).unwrap_err();
            format!("{error:#}")
        };
        // Truncated in a member, after one and in a zstd frame, and a broken header.
        let file = member("alpine/file", b'0', "", &[1; 600]);
        let mut bytes = finish(std::slice::from_ref(&file));
        rejected("member", &bytes[..700]);
        assert!(rejected("boundary", &file).contains("truncated"));
        assert!(rejected("nothing", b"").contains("truncated"));
        let mut compressed = compress_to_vec(file.as_slice(), CompressionLevel::Fastest);
        assert!(rejected("frame", &compressed).contains("truncated"));
        compressed.extend(compress_to_vec(
            &[0; 2 * BLOCK][..],
            CompressionLevel::Fastest,
        ));
        extract(compressed.as_slice(), &dir.join("complete")).unwrap();
        rejected("in-frame", &compressed[..compressed.len() - 5]);
        bytes[0] ^= 1;
        rejected("checksum", &bytes);
        // A file with another file's name as its directory, and the other way around.
        let conflicts = [
            [member("a", b'0', "", b""), member("a/b", b'0', "", b"")],
            [member("a/b", b'0', "", b""), member("a", b'0', "", b"")],
        ];
        for (index, members) in conflicts.iter().enumerate() {
            rejected(&format!("conflict-{index}"), &finish(members));
        }
        // Sparse files in the pax format (of each version).
        let sparse = [
            [("GNU.sparse.major", "1"), ("GNU.sparse.minor", "0")],
            [("GNU.sparse.size", "4"), ("GNU.sparse.map", "0,4")],
            [("GNU.sparse.offset", "0"), ("GNU.sparse.numbytes", "4")],
        ];
        for (index, records) in sparse.iter().enumerate() {
            let archive = finish(&[pax(records), member("alpine/disk", b'0', "", b"data")]);
            let error = rejected(&format!("sparse-{index}"), &archive);
            assert!(error.contains("sparse"), "{error}");
        }
        // Broken zstd.
        let mut compressed = vec![0x28, 0xb5, 0x2f, 0xfd];
        compressed.extend([0xff; 16]);
        rejected("zstd", &compressed);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
