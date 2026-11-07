//! Starting session processes. We fork and exec ourselves (instead of using
//! std::process::Command) because the reaper owns all wait statuses.

use std::ffi::CString;
use std::os::fd::RawFd;

use super::sys::{Error, cstring};

/// The stdin / stdout / stderr of a new process.
pub enum Stdio {
    /// The slave side of a pseudo terminal, which becomes the controlling terminal.
    Pty(RawFd),
    Pipes {
        stdin: RawFd,
        stdout: RawFd,
        stderr: RawFd,
    },
}

/// Looks up a program in PATH (unless it contains a slash).
pub fn resolve(program: &str, path: &str) -> Option<String> {
    if program.contains('/') {
        return Some(program.to_string());
    }
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .find_map(|dir| {
            let candidate = format!("{dir}/{program}");
            let c = cstring(&candidate);
            // SAFETY: The path is a NUL-terminated string.
            (unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0).then_some(candidate)
        })
}

/// Starts `path` with `argv` (including argv[0]) and `env` ("NAME=value") in a new
/// session. Returns the PID; the reaper reports its status.
pub fn spawn(
    path: &str,
    argv: &[String],
    env: &[String],
    cwd: &str,
    stdio: &Stdio,
) -> Result<i32, Error> {
    // Everything that the child needs is prepared before fork(): after it, the child
    // may only call async-signal-safe functions.
    let path_c = cstring(path);
    let argv_c: Vec<CString> = argv.iter().map(|a| cstring(a)).collect();
    let env_c: Vec<CString> = env.iter().map(|e| cstring(e)).collect();
    let cwd_c = cstring(cwd);
    let mut argv_p: Vec<*const libc::c_char> = argv_c.iter().map(|a| a.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut env_p: Vec<*const libc::c_char> = env_c.iter().map(|e| e.as_ptr()).collect();
    env_p.push(std::ptr::null());
    // The agent blocks SIGCHLD (see reaper::block_sigchld); the mask survives exec.
    // SAFETY: sigemptyset() initializes the set.
    let no_signals = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        set
    };

    // The child reports a failed exec through this pipe (it closes on a successful exec).
    let mut status_pipe = [0; 2];
    // SAFETY: pipe2() writes two file descriptors.
    if unsafe { libc::pipe2(status_pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(Error::last("pipe2()"));
    }
    let [status_read, status_write] = status_pipe;

    // SAFETY: The child only performs async-signal-safe calls before exec / _exit.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = std::io::Error::last_os_error();
        // SAFETY: We own the pipe.
        unsafe {
            libc::close(status_read);
            libc::close(status_write);
        }
        return Err(Error::io("fork()", error));
    }
    if pid == 0 {
        // SAFETY: Only async-signal-safe calls (see above).
        unsafe {
            libc::setsid();
            match *stdio {
                Stdio::Pty(slave) => {
                    libc::ioctl(slave, libc::TIOCSCTTY, 0);
                    for target in 0..3 {
                        libc::dup2(slave, target);
                    }
                }
                Stdio::Pipes {
                    stdin,
                    stdout,
                    stderr,
                } => {
                    libc::dup2(stdin, 0);
                    libc::dup2(stdout, 1);
                    libc::dup2(stderr, 2);
                }
            }
            // Rust ignores SIGPIPE; the ignored disposition would survive exec.
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::sigprocmask(libc::SIG_SETMASK, &no_signals, std::ptr::null_mut());
            if libc::chdir(cwd_c.as_ptr()) != 0 {
                libc::chdir(c"/".as_ptr());
            }
            libc::execve(path_c.as_ptr(), argv_p.as_ptr(), env_p.as_ptr());
            let errno = *libc::__errno_location();
            libc::write(status_write, (&raw const errno).cast(), 4);
            libc::_exit(127);
        }
    }

    // SAFETY: We own the write end; reading the errno is plain I/O.
    let errno = unsafe {
        libc::close(status_write);
        let mut errno = 0i32;
        let count = libc::read(status_read, (&raw mut errno).cast(), 4);
        libc::close(status_read);
        (count == 4).then_some(errno)
    };
    if let Some(errno) = errno {
        // Nobody registers the child, so the reaper drops its status.
        return Err(Error::io(path, std::io::Error::from_raw_os_error(errno)));
    }
    Ok(pid)
}

/// Converts a wait status to (exit code, signal).
pub fn exit_status(status: i32) -> (Option<i32>, Option<i32>) {
    if libc::WIFEXITED(status) {
        (Some(libc::WEXITSTATUS(status)), None)
    } else if libc::WIFSIGNALED(status) {
        (None, Some(libc::WTERMSIG(status)))
    } else {
        (None, None)
    }
}
