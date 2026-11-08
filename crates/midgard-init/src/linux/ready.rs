//! Waiting for the distribution's init to finish booting (see [`Ready`]), such that
//! sessions find its services running (e.g., D-Bus). The watcher ([`watch`]) learns it
//! in the way that the guest configuration says; sessions [`wait`] for it.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_io::{Async, Timer};
use event_listener::Event;
use futures_lite::future;
use midgard_protocol::guest::Ready;

use super::{PATH, RUN_DIR, log, process, reaper};

/// How long sessions wait for the distribution's init; then they run anyway.
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);

/// The socket that `midgard-init ready` connects to (with [`Ready::Notify`]).
const NOTIFY_SOCKET: &str = "/run/midgard/notify";

/// Whether the guest finished booting (or we gave up waiting).
static BOOTED: AtomicBool = AtomicBool::new(false);
static BOOTED_EVENT: Event = Event::new();

/// Waits until the guest finished booting.
pub async fn wait() {
    loop {
        if BOOTED.load(Ordering::SeqCst) {
            return;
        }
        let listener = BOOTED_EVENT.listen();
        if BOOTED.load(Ordering::SeqCst) {
            return;
        }
        listener.await;
    }
}

/// What the watcher waits for. It is prepared before the distribution's init starts,
/// such that the guest cannot notify us before we listen.
pub enum Watcher {
    Immediately,
    Systemd,
    Notify(UnixListener),
}

impl Watcher {
    pub fn new(ready: Ready) -> Self {
        match ready {
            Ready::Immediately => Watcher::Immediately,
            Ready::Systemd => Watcher::Systemd,
            Ready::Notify => {
                let _ = std::fs::remove_file(NOTIFY_SOCKET);
                match UnixListener::bind(NOTIFY_SOCKET) {
                    Ok(listener) => Watcher::Notify(listener),
                    Err(error) => {
                        log!("listening on {NOTIFY_SOCKET}: {error}");
                        Watcher::Immediately
                    }
                }
            }
        }
    }
}

/// Watches the guest boot (from the agent's start); see [`wait`].
pub async fn watch(watcher: Watcher) {
    let start = Instant::now();
    let booted = match watcher {
        Watcher::Immediately => None,
        Watcher::Systemd => {
            log!("waiting for systemd to finish booting");
            Some(systemd_booted(start + BOOT_TIMEOUT).await)
        }
        Watcher::Notify(listener) => {
            log!("waiting for the guest to run {RUN_DIR}/midgard-init ready");
            let booted = notified(listener).await;
            let _ = std::fs::remove_file(NOTIFY_SOCKET);
            Some(booted)
        }
    };
    match booted {
        Some(true) => log!("booted after {} ms", start.elapsed().as_millis()),
        // (systemd also reports a degraded system, i.e., failed units, as an error.)
        Some(false) => log!(
            "the guest did not finish booting within {} s, or some services failed",
            BOOT_TIMEOUT.as_secs()
        ),
        None => {}
    }
    BOOTED.store(true, Ordering::SeqCst);
    BOOTED_EVENT.notify(usize::MAX);
}

async fn systemd_booted(deadline: Instant) -> bool {
    // Until systemd listens on its private socket, systemctl fails instead of waiting.
    while !Path::new("/run/systemd/private").exists() {
        if Instant::now() > deadline {
            return false;
        }
        Timer::after(Duration::from_millis(50)).await;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    run_with_timeout(&["systemctl", "is-system-running", "--wait"], remaining).await
}

/// Waits for `midgard-init ready` to connect.
async fn notified(listener: UnixListener) -> bool {
    let listener = match Async::new(listener) {
        Ok(listener) => listener,
        Err(error) => {
            log!("{NOTIFY_SOCKET}: {error}");
            return false;
        }
    };
    let accept = async { listener.accept().await.is_ok() };
    future::or(accept, async {
        Timer::after(BOOT_TIMEOUT).await;
        false
    })
    .await
}

/// Runs a program (in PATH) and returns whether it exited with 0 within the timeout.
async fn run_with_timeout(argv: &[&str], timeout: Duration) -> bool {
    let Some(path) = process::resolve(argv[0], PATH) else {
        log!("{}: command not found", argv[0]);
        return false;
    };
    let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
    let env = [format!("PATH={PATH}")];
    let result = process::dev_null().and_then(|null| {
        let null = null.as_raw_fd();
        let stdio = process::Stdio::Pipes {
            stdin: null,
            stdout: null,
            stderr: null,
        };
        process::spawn(&path, &argv, &env, "/", &stdio)
    });
    match result {
        Ok(pid) => {
            let exited = reaper::watch(pid);
            let status = future::or(async { exited.recv().await.ok() }, async {
                Timer::after(timeout).await;
                None
            })
            .await;
            if status.is_none() {
                // SAFETY: kill() only takes numbers.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            status.is_some_and(|status| libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0)
        }
        Err(error) => {
            log!("{error}");
            false
        }
    }
}

/// `midgard-init ready`: tells the agent that the guest finished booting.
pub fn notify() -> io::Result<()> {
    match UnixStream::connect(NOTIFY_SOCKET) {
        // The agent does not wait (anymore), e.g., because the guest notified it before.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result.map(drop),
    }
}
