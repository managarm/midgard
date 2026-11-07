//! As PID 1, we reap every process. Sessions register the processes that they started
//! and get their wait status; the statuses of other processes (e.g., orphans that the
//! kernel reparented to us) are dropped. SIGCHLD arrives through a signalfd.
//!
//! The reaper runs as a task on the agent's only thread, and sessions register a process
//! before they await anything after starting it, so the reaper never sees a process
//! before it is registered.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::sync::Mutex;

use async_channel::{Receiver, Sender};
use async_io::Async;

use super::log;

/// The registered processes and where their wait statuses go.
static WAITERS: Mutex<Option<HashMap<i32, Sender<i32>>>> = Mutex::new(None);

/// Returns a channel that receives the wait status of `pid` (see the module
/// documentation for when to call it).
pub fn watch(pid: i32) -> Receiver<i32> {
    let (sender, receiver) = async_channel::bounded(1);
    let mut waiters = WAITERS.lock().unwrap();
    waiters.get_or_insert_with(HashMap::new).insert(pid, sender);
    receiver
}

fn sigchld() -> libc::sigset_t {
    // SAFETY: sigemptyset() and sigaddset() initialize the set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        set
    }
}

/// Blocks SIGCHLD, such that it only arrives through the reaper's signalfd. This must
/// happen before any thread starts (threads inherit the mask); children unblock it.
pub fn block_sigchld() {
    // SAFETY: pthread_sigmask() reads the set.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &sigchld(), std::ptr::null_mut()) };
}

/// Collects the statuses of all processes that exited.
fn reap() {
    loop {
        let mut status = 0;
        // SAFETY: waitpid() only writes the status.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        // 0: no child exited (yet); -1: no children (ECHILD).
        if pid <= 0 {
            return;
        }
        let mut waiters = WAITERS.lock().unwrap();
        if let Some(sender) = waiters.as_mut().and_then(|waiters| waiters.remove(&pid)) {
            let _ = sender.try_send(status);
        }
    }
}

/// Reaps processes whenever SIGCHLD arrives (after [`block_sigchld`]).
pub async fn run() {
    // SAFETY: signalfd() reads the set.
    let fd = unsafe { libc::signalfd(-1, &sigchld(), libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
    if fd < 0 {
        log!("signalfd(): {}", std::io::Error::last_os_error());
        return;
    }
    // SAFETY: We own the new file descriptor.
    let signals = match Async::new(unsafe { File::from_raw_fd(fd) }) {
        Ok(signals) => signals,
        Err(error) => {
            log!("signalfd: {error}");
            return;
        }
    };
    let mut info = [0u8; 16 * size_of::<libc::signalfd_siginfo>()];
    loop {
        // Pending signals merge; one SIGCHLD can stand for several processes.
        while signals
            .get_ref()
            .read(&mut info)
            .is_ok_and(|count| count > 0)
        {}
        reap();
        if let Err(error) = signals.readable().await {
            log!("signalfd: {error}");
            return;
        }
    }
}
