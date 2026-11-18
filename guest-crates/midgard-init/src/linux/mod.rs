//! Boot: mount the root file system, move into it, start the agent (which serves
//! sessions) in a child process and the distribution's init (if the guest configuration
//! names one).

mod display;
mod sys;

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use midgard_protocol::guest::{
    self, AGENT_FILE, AGENT_PATH, GuestConfig, HELPER_PATH, PRIVATE_DIR,
};
use midgard_protocol::version::PROTOCOL;
use midgard_protocol::{CONFIG_PORT, HOST_CID, SHARE_MOUNT, SHARE_TAG, param};
use sys::{Error, mount};

/// The PATH of the commands that we run.
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The root file system's device unless the kernel command line names one: the first
/// disk.
const DEFAULT_ROOT: &str = "/dev/vda";

/// Logs a line to the console, in one write: the distribution's init writes there, too.
macro_rules! log {
    ($($arg:tt)*) => {{
        let line = format!("midgard-init: {}\n", format_args!($($arg)*));
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    }};
}
pub(crate) use log;

pub fn main() {
    // (We ignore our arguments: the kernel passes the parameters that it does not know,
    // e.g., "single".)
    if std::process::id() != 1 {
        eprintln!("midgard-init: only runs as PID 1 (the guest runs {HELPER_PATH})");
        std::process::exit(1);
    }
    if let Err(error) = boot() {
        log!("{error:#}");
        // PID 1 must not exit; stop the VM instead (midgard sees all vCPUs halt).
        sys::power_off();
    }
}

/// Returns the value of `name=` on the kernel command line (of the last one, such that
/// what is appended to the command line overrides what is in front of it).
fn parameter(cmdline: &str, name: &str) -> Option<String> {
    cmdline
        .split_whitespace()
        .rev()
        .find_map(|p| p.strip_prefix(name)?.strip_prefix('=').map(str::to_string))
}

fn boot() -> Result<()> {
    // In the initramfs: devices and /proc, then the console as stdin / stdout / stderr.
    for dir in ["/dev", "/proc", "/sys", "/newroot"] {
        let _ = std::fs::create_dir(dir);
    }
    mount("devtmpfs", "/dev", "devtmpfs", 0, Some("mode=0755"))?;
    mount("proc", "/proc", "proc", 0, None)?;
    sys::open_console();

    // First of all, whether midgard and we fit together: the rest depends on it.
    let config = fetch_config()?;
    let cmdline = std::fs::read_to_string("/proc/cmdline").context("reading /proc/cmdline")?;
    let root = parameter(&cmdline, param::ROOT).unwrap_or_else(|| DEFAULT_ROOT.into());
    let fstype = parameter(&cmdline, param::ROOT_FSTYPE).unwrap_or_else(|| "ext4".into());
    let agent = load_agent()?;
    let waypipe = display::load();

    // The disk shows up once its driver probed it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::path::Path::new(&root).exists() {
        if Instant::now() > deadline {
            bail!("{root} does not exist");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    mount(&root, "/newroot", &fstype, 0, None)?;

    // Move /dev and /proc into the new root and make it the root (like switch_root;
    // the initramfs stays in memory, but only we are left in it).
    for dir in ["/dev", "/proc"] {
        let target = format!("/newroot{dir}");
        let _ = std::fs::create_dir(&target);
        mount(dir, &target, "", libc::MS_MOVE, None)?;
    }
    sys::chdir("/newroot")?;
    mount(".", "/", "", libc::MS_MOVE, None)?;
    sys::chroot(".")?;
    sys::chdir("/")?;

    setup_root();
    setup_network();
    mount_share();
    install_agent(&agent);
    drop(agent);
    if let Some(waypipe) = &waypipe {
        display::install(waypipe, &config);
    }
    drop(waypipe);

    let agent_pid = start_agent()?;
    // With the distribution's init, it becomes PID 1 (and the agent its child).
    if let Some(init) = &config.init {
        exec_init(init);
    }
    reap(agent_pid)
}

/// Fetches the guest configuration from midgard. Fails if midgard speaks no version of
/// the guest protocol that we speak, or if we do not get its configuration. (The agent
/// fetches it, too.)
fn fetch_config() -> Result<GuestConfig> {
    // The vsock device shows up once its driver probed it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match sys::vsock_connect(HOST_CID, CONFIG_PORT) {
            Ok(stream) => break stream,
            // (A midgard from before we fetched the configuration does not answer.)
            Err(error) if Instant::now() > deadline => {
                bail!("cannot reach midgard for the guest configuration ({error})")
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let (version, config) = guest::fetch_config(&mut stream)?;
    if version != PROTOCOL {
        log!("midgard is older: speaking version {version} of the guest protocol");
    }
    Ok(config)
}

/// Reads midgard-agent from the initramfs (before we leave it).
fn load_agent() -> Result<Vec<u8>> {
    let path = format!("/{AGENT_FILE}");
    let agent = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    // The initramfs stays in memory.
    let _ = std::fs::remove_file(&path);
    Ok(agent)
}

/// Installs the agent as [`AGENT_PATH`] and the name under which the guest runs it
/// ([`HELPER_PATH`]).
fn install_agent(agent: &[u8]) {
    // (Users run the helper and waypipe.)
    let result = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(PRIVATE_DIR)
        .and_then(|()| std::fs::write(AGENT_PATH, agent))
        .and_then(|()| std::fs::set_permissions(AGENT_PATH, PermissionsExt::from_mode(0o755)));
    if let Err(error) = result {
        log!("installing {AGENT_PATH}: {error}");
    }
    if let Err(error) = std::os::unix::fs::symlink(AGENT_PATH, HELPER_PATH) {
        log!("creating {HELPER_PATH}: {error}");
    }
}

/// Starts the agent in a child process (in a session of its own), such that it is never
/// PID 1 and only waits for its own children. Returns its PID.
fn start_agent() -> Result<libc::pid_t> {
    let path = sys::cstring(AGENT_PATH);
    let path_env = sys::cstring(&format!("PATH={PATH}"));
    let argv = [
        c"midgard-agent".as_ptr(),
        c"serve".as_ptr(),
        c"--from-midgard-init".as_ptr(),
        std::ptr::null(),
    ];
    let env = [
        c"HOME=/".as_ptr(),
        c"TERM=linux".as_ptr(),
        path_env.as_ptr(),
        std::ptr::null(),
    ];
    // SAFETY: fork() in a single-threaded process (we are, until we execute the
    // distribution's init).
    match unsafe { libc::fork() } {
        -1 => bail!("fork(): {}", std::io::Error::last_os_error()),
        0 => {
            // SAFETY: setsid() has no preconditions; argv and env are NULL-terminated
            // arrays of NUL-terminated strings.
            unsafe {
                libc::setsid();
                libc::execve(path.as_ptr(), argv.as_ptr(), env.as_ptr());
            }
            log!(
                "executing {AGENT_PATH}: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: _exit() has no preconditions.
            unsafe { libc::_exit(127) }
        }
        pid => Ok(pid),
    }
}

/// Replaces PID 1 with the distribution's init. Returns if that fails.
fn exec_init(init: &str) {
    log!("starting {init}");
    // Rust ignores SIGPIPE; the ignored disposition would survive exec.
    // SAFETY: signal() only takes numbers.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let path = sys::cstring(init);
    let argv = [path.as_ptr(), std::ptr::null()];
    let env = [c"HOME=/".as_ptr(), c"TERM=linux".as_ptr(), std::ptr::null()];
    // SAFETY: argv and env are NULL-terminated arrays of NUL-terminated strings.
    unsafe { libc::execve(path.as_ptr(), argv.as_ptr(), env.as_ptr()) };
    log!("executing {init}: {}", std::io::Error::last_os_error());
}

/// PID 1 without the distribution's init: reaps orphans until the agent exits, then
/// powers off (midgard sees the VM stop).
fn reap(agent: libc::pid_t) -> ! {
    loop {
        let mut status = 0;
        // SAFETY: waitpid() only writes the status.
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            std::thread::sleep(Duration::from_secs(1));
        } else if pid == agent {
            if libc::WIFSIGNALED(status) {
                log!("the agent was killed by signal {}", libc::WTERMSIG(status));
            } else {
                log!("the agent exited with {}", libc::WEXITSTATUS(status));
            }
            sys::power_off();
        }
    }
}

/// Mounts that every distribution expects.
fn setup_root() {
    let mounts: [(&str, &str, &str, Option<&str>); 4] = [
        ("sysfs", "/sys", "sysfs", None),
        (
            "devpts",
            "/dev/pts",
            "devpts",
            Some("gid=5,mode=620,ptmxmode=666"),
        ),
        ("tmpfs", "/dev/shm", "tmpfs", Some("mode=1777")),
        ("tmpfs", "/run", "tmpfs", Some("mode=0755")),
    ];
    for (source, target, fstype, options) in mounts {
        let _ = std::fs::create_dir_all(target);
        if let Err(error) = mount(source, target, fstype, 0, options) {
            log!("{error}");
        }
    }
    // Some distributions expect /dev/ptmx to be the devpts one.
    if !std::path::Path::new("/dev/ptmx").exists() {
        let _ = std::os::unix::fs::symlink("pts/ptmx", "/dev/ptmx");
    }
    let hostname = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty() && h != "localhost")
        .unwrap_or_else(|| "midgard".into());
    sys::set_hostname(&hostname);
}

/// The kernel configured the interface (ip=); we bring up the loopback interface and
/// tell the C library about the DNS server (the kernel writes it to /proc/net/pnp).
fn setup_network() {
    if let Err(error) = sys::interface_up("lo") {
        log!("{error}");
    }
    if let Ok(pnp) = std::fs::read_to_string("/proc/net/pnp") {
        let nameservers: String = pnp
            .lines()
            .filter(|line| line.starts_with("nameserver") && !line.ends_with(" 0.0.0.0"))
            .map(|line| format!("{line}\n"))
            .collect();
        if !nameservers.is_empty() {
            // /etc/resolv.conf may be a dangling symlink (e.g., to systemd-resolved).
            let _ = std::fs::remove_file("/etc/resolv.conf");
            let content = format!("# Generated by midgard-init.\n{nameservers}");
            if let Err(error) = std::fs::write("/etc/resolv.conf", content) {
                log!("writing /etc/resolv.conf: {error}");
            }
        }
    }
}

/// The host's shared directory (virtio-fs), if midgard offers one.
fn mount_share() {
    let _ = std::fs::create_dir_all(SHARE_MOUNT);
    match mount(SHARE_TAG, SHARE_MOUNT, "virtiofs", 0, None) {
        Ok(()) => {}
        // No virtio-fs device with that tag (or no virtio-fs at all).
        Err(Error::Os { error, .. })
            if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENODEV)) => {}
        Err(error) => log!("{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters() {
        let cmdline = "console=hvc0 midgard.root=/dev/vda quiet midgard.root=/dev/vdb\n";
        assert_eq!(parameter(cmdline, param::ROOT).as_deref(), Some("/dev/vdb"));
        assert_eq!(parameter(cmdline, "console").as_deref(), Some("hvc0"));
        assert_eq!(parameter(cmdline, param::ROOT_FSTYPE), None);
        assert_eq!(parameter(cmdline, "quiet"), None);
    }
}
