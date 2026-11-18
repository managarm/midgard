//! Thin wrappers around the system calls that we need.

use std::ffi::CString;
use std::fs::File;
use std::os::fd::{FromRawFd, RawFd};

/// An error that the agent handles (e.g., by reporting it to the client).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A system call failed; `what` describes it.
    #[error("{what}: {error}")]
    Os { what: String, error: std::io::Error },
    #[error("{0}")]
    Message(String),
}

impl Error {
    pub fn io(what: impl Into<String>, error: std::io::Error) -> Self {
        Error::Os {
            what: what.into(),
            error,
        }
    }

    pub fn last(what: impl Into<String>) -> Self {
        Self::io(what, std::io::Error::last_os_error())
    }
}

pub fn cstring(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|_| CString::new("?").unwrap())
}

pub fn mount(
    source: &str,
    target: &str,
    fstype: &str,
    flags: u64,
    data: Option<&str>,
) -> Result<(), Error> {
    let source_c = cstring(source);
    let target_c = cstring(target);
    let fstype_c = cstring(fstype);
    let data_c = data.map(cstring);
    // SAFETY: All pointers are NUL-terminated strings (or null).
    let result = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            if fstype.is_empty() {
                std::ptr::null()
            } else {
                fstype_c.as_ptr()
            },
            flags as libc::c_ulong,
            data_c
                .as_ref()
                .map_or(std::ptr::null(), |d| d.as_ptr().cast()),
        )
    };
    if result != 0 {
        return Err(Error::last(format!("mounting {source} on {target}")));
    }
    Ok(())
}

/// Syncs and powers off (through ACPI; midgard then stops).
pub fn power_off() -> ! {
    // SAFETY: These calls have no memory-safety preconditions.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Connects to the vsock port `port` of `cid`.
pub fn vsock_connect(cid: u32, port: u32) -> Result<File, Error> {
    // SAFETY: socket() and connect() get a valid address; we own the new socket.
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(Error::last("socket(AF_VSOCK)"));
        }
        let stream = File::from_raw_fd(fd);
        let mut address: libc::sockaddr_vm = std::mem::zeroed();
        address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        address.svm_port = port;
        address.svm_cid = cid;
        let result = libc::connect(
            fd,
            (&raw const address).cast(),
            size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        );
        if result != 0 {
            return Err(Error::last(format!("connecting to vsock port {port}")));
        }
        Ok(stream)
    }
}

/// A listening vsock stream socket on `port` (any CID).
pub fn vsock_listen(port: u32) -> Result<RawFd, Error> {
    // SAFETY: socket(), bind() and listen() get a valid address.
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(Error::last("socket(AF_VSOCK)"));
        }
        let mut address: libc::sockaddr_vm = std::mem::zeroed();
        address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        address.svm_port = port;
        address.svm_cid = libc::VMADDR_CID_ANY;
        let result = libc::bind(
            fd,
            (&raw const address).cast(),
            size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        );
        if result != 0 {
            let error = Error::last(format!("binding vsock port {port}"));
            libc::close(fd);
            return Err(error);
        }
        if libc::listen(fd, 64) != 0 {
            let error = Error::last("listen()");
            libc::close(fd);
            return Err(error);
        }
        Ok(fd)
    }
}

/// A pidfd for `pid` (or -1 with errno set). Linux's musl lacks the wrapper; Managarm has
/// it, but no syscall().
pub fn pidfd_open(pid: i32) -> RawFd {
    // SAFETY: pidfd_open() only takes numbers.
    #[cfg(target_os = "linux")]
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
    // SAFETY: pidfd_open() only takes numbers.
    #[cfg(target_os = "managarm")]
    let pidfd = unsafe { libc::pidfd_open(pid, 0) };
    pidfd
}

/// Sends `signal` to the process of `pidfd`.
pub fn pidfd_send_signal(pidfd: RawFd, signal: i32) {
    // SAFETY: pidfd_send_signal() takes no siginfo_t (null) and no flags.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    // SAFETY: pidfd_send_signal() takes no siginfo_t (null) and no flags.
    #[cfg(target_os = "managarm")]
    unsafe {
        libc::pidfd_send_signal(pidfd, signal, std::ptr::null_mut(), 0)
    };
}

pub fn accept(listener: RawFd) -> std::io::Result<RawFd> {
    // SAFETY: We do not need the peer's address.
    let fd = unsafe {
        libc::accept4(
            listener,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}
