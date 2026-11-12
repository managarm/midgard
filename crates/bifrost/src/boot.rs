//! `bifrost boot`: boots a VM in the foreground (for debugging). bifrost decides the
//! VM's configuration and asks midgard (on midgard's socket) to run it. Our stdin feeds
//! the VM's console; if it is a terminal, it is switched to raw mode and Ctrl-A x quits,
//! like in QEMU: midgard stops the VM, and we keep showing its console until it is
//! gone. A second Ctrl-A x (within [`FORCE_QUIT`]) exits right away.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use midgard_protocol::control::{Request, Response};
use midgard_protocol::guest::GuestConfig;
use midgard_protocol::machine::{ConsoleKind, Disk, MachineConfig};

use crate::cli::{BootArgs, Console};
use crate::{client, terminal};

/// How soon a second Ctrl-A x must follow the first one to exit right away.
const FORCE_QUIT: Duration = Duration::from_secs(1);

/// The guest's RAM unless --memory says otherwise.
const DEFAULT_MEMORY: u64 = 1 << 30;

const CTRL_A: u8 = 0x01;

/// `path` for midgard, which does not run in our working directory.
fn absolute(path: &Path) -> Result<std::path::PathBuf> {
    std::path::absolute(path).with_context(|| format!("invalid path {}", path.display()))
}

/// The VM that the command line describes.
fn config(boot: &BootArgs) -> Result<MachineConfig> {
    let disks = boot
        .disk
        .iter()
        .map(|disk| {
            Ok(Disk {
                path: absolute(&disk.path)?,
                read_only: disk.read_only,
            })
        })
        .collect::<Result<_>>()?;
    Ok(MachineConfig {
        kernel: absolute(&boot.kernel)?,
        initrd: boot.initrd.as_deref().map(absolute).transpose()?,
        agent: boot.agent.as_deref().map(absolute).transpose()?,
        cmdline: boot.cmdline.clone(),
        append: boot.append.clone(),
        memory: boot.memory.unwrap_or(DEFAULT_MEMORY),
        cpus: boot.cpus,
        console: match boot.console {
            Console::Virtio => ConsoleKind::Virtio,
            Console::Serial => ConsoleKind::Serial,
        },
        disks,
        network: !boot.no_network,
        share: match (&boot.share, boot.no_share) {
            (_, true) => None,
            (Some(share), false) => Some(absolute(share)?),
            (None, false) => Some("/".into()),
        },
        // The agent stays PID 1.
        guest: GuestConfig::default(),
        waypipe: boot.waypipe.as_deref().map(absolute).transpose()?,
    })
}

/// Interprets the escape sequences in `input` (with `escape`): Ctrl-A x quits, Ctrl-A
/// Ctrl-A sends Ctrl-A. Returns the input for the guest (up to the first Ctrl-A x) and
/// how many times to quit; `after_ctrl_a` carries a Ctrl-A at the end of `input` over to
/// the next call.
fn unescape(input: &[u8], escape: bool, after_ctrl_a: &mut bool) -> (Vec<u8>, usize) {
    if !escape {
        return (input.to_vec(), 0);
    }
    let mut data = Vec::with_capacity(input.len());
    // The length of `data` at the first Ctrl-A x (input after it is discarded).
    let mut quit_at = None;
    let mut quits = 0;
    for &byte in input {
        if std::mem::take(after_ctrl_a) {
            match byte {
                b'x' => {
                    quit_at.get_or_insert(data.len());
                    quits += 1;
                }
                // Ctrl-A Ctrl-A sends Ctrl-A; other sequences send both bytes.
                CTRL_A => data.push(CTRL_A),
                _ => data.extend([CTRL_A, byte]),
            }
        } else if byte == CTRL_A {
            *after_ctrl_a = true;
        } else {
            data.push(byte);
        }
    }
    if let Some(length) = quit_at {
        data.truncate(length);
    }
    (data, quits)
}

/// Whether a Ctrl-A x at `now` exits right away, after the first one at `first`.
fn force_quit(first: Option<Instant>, now: Instant) -> bool {
    first.is_some_and(|first| now.duration_since(first) <= FORCE_QUIT)
}

/// Forwards stdin to the VM's console until stdin ends. With `escape`, Ctrl-A x closes
/// our side of the connection (midgard then stops the VM); input after that is
/// discarded, except that a second Ctrl-A x soon after exits.
fn forward_input(mut stream: UnixStream, escape: bool) {
    let mut stdin = io::stdin().lock();
    let mut buffer = [0u8; 256];
    let mut after_ctrl_a = false;
    let mut quit_at = None;
    loop {
        let count = match stdin.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        let (data, quits) = unescape(&buffer[..count], escape, &mut after_ctrl_a);
        if quit_at.is_none()
            && !data.is_empty()
            && midgard_protocol::write_message(&mut stream, &Request::Input(data)).is_err()
        {
            return;
        }
        for _ in 0..quits {
            let now = Instant::now();
            if force_quit(quit_at, now) {
                terminal::restore();
                eprintln!("bifrost: quit (midgard stops the VM)");
                std::process::exit(0);
            }
            if quit_at.is_none() {
                let _ = stream.shutdown(Shutdown::Write);
            }
            quit_at = Some(now);
        }
    }
}

/// Asks midgard to boot the VM and connects our stdin / stdout to its console until it
/// stops (or we quit, or are killed, which stops it).
pub fn boot(args: &BootArgs) -> Result<()> {
    let config = config(args)?;
    let mut stream = client::connect_daemon()?;
    midgard_protocol::write_message(&mut stream, &Request::Boot(Box::new(config)))?;
    // The guest's terminal does the output processing. We restore ours before we print
    // anything (main() does on errors).
    let escape = terminal::enter_raw_mode(false);
    let input = stream.try_clone()?;
    std::thread::spawn(move || forward_input(input, escape));
    let mut stdout = io::stdout().lock();
    loop {
        match midgard_protocol::read_message(&mut stream)? {
            Some(Response::Console(data)) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Some(Response::Stopped(reason)) => {
                terminal::restore();
                eprintln!("bifrost: {reason}");
                return Ok(());
            }
            Some(Response::Error(error)) => bail!("{error}"),
            Some(other) => bail!("unexpected answer from midgard: {other:?}"),
            None => bail!("midgard closed the connection"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Action, Cli};

    #[test]
    fn defaults() {
        let cli = Cli::try_parse_args(["bifrost", "boot", "--kernel", "bzImage"]).unwrap();
        let Some(Action::Boot(boot)) = cli.action else {
            panic!("not boot: {:?}", cli.action);
        };
        let config = config(&boot).unwrap();
        assert!(config.kernel.is_absolute());
        assert!(config.kernel.ends_with("bzImage"));
        assert_eq!(config.initrd, None);
        assert_eq!(config.agent, None);
        assert_eq!(config.cmdline, None);
        assert!(config.append.is_empty());
        assert_eq!(config.memory, DEFAULT_MEMORY);
        assert_eq!(config.cpus, None);
        assert_eq!(config.console, ConsoleKind::Virtio);
        assert!(config.disks.is_empty());
        assert!(config.network);
        assert_eq!(config.share, Some("/".into()));
        assert_eq!(config.guest, GuestConfig::default());
        assert_eq!(config.waypipe, None);
    }

    #[test]
    fn second_quit_within_a_second() {
        let first = Instant::now();
        assert!(!force_quit(None, first));
        assert!(force_quit(Some(first), first + FORCE_QUIT / 2));
        assert!(!force_quit(Some(first), first + FORCE_QUIT * 2));
    }

    #[test]
    fn escape_sequences() {
        let mut after_ctrl_a = false;
        assert_eq!(
            unescape(b"ls\r\x01\x01a\x01b", true, &mut after_ctrl_a),
            (b"ls\r\x01a\x01b".to_vec(), 0)
        );
        // Ctrl-A x, across reads.
        assert_eq!(
            unescape(b"q\x01", true, &mut after_ctrl_a),
            (b"q".to_vec(), 0)
        );
        assert!(after_ctrl_a);
        assert_eq!(unescape(b"xyz", true, &mut after_ctrl_a), (Vec::new(), 1));
        // Twice in one read.
        assert_eq!(
            unescape(b"a\x01xb\x01x", true, &mut after_ctrl_a),
            (b"a".to_vec(), 2)
        );
        // Without a terminal, everything goes to the guest.
        let mut after_ctrl_a = false;
        assert_eq!(
            unescape(b"\x01x", false, &mut after_ctrl_a),
            (b"\x01x".to_vec(), 0)
        );
    }
}
