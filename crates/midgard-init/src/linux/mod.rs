//! Boot: mount the root file system, move into it, start the distribution's init (if
//! the guest configuration names one) and serve sessions.

mod display;
mod launcher;
mod process;
mod ready;
mod reaper;
mod rt;
mod session;
mod sys;
mod users;

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_io::{Async, Timer};
use midgard_protocol::guest::{CONFIG_FILE, Display, GuestConfig};
use midgard_protocol::{AGENT_PORT, SHARE_MOUNT, SHARE_TAG, param};
use sys::{Error, mount};

/// The default PATH of sessions.
pub const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Where we put files for the guest (a tmpfs, see [`setup_root`]).
pub const RUN_DIR: &str = "/run/midgard";
/// Our executable in the guest, which the guest runs in other modes (see [`main`]).
pub const HELPER: &str = "/run/midgard/midgard-init";

/// The guest configuration (from the initramfs).
static CONFIG: OnceLock<GuestConfig> = OnceLock::new();

pub fn config() -> &'static GuestConfig {
    CONFIG.get_or_init(GuestConfig::default)
}

/// Whether we started the distribution's init (and are its child).
static INIT_RUNS: AtomicBool = AtomicBool::new(false);

/// Whether the distribution's init runs as PID 1 (and we are its child).
pub fn init_runs() -> bool {
    // If executing the init failed, PID 1 is still us.
    INIT_RUNS.load(Ordering::Relaxed) && !same_file("/proc/1/exe", "/proc/self/exe")
}

fn same_file(a: &str, b: &str) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

/// Logs a line to the console, in one write: the distribution's init writes there, too.
macro_rules! log {
    ($($arg:tt)*) => {{
        let line = format!("midgard-init: {}\n", format_args!($($arg)*));
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    }};
}
pub(crate) use log;

pub fn main() {
    // (As PID 1, we ignore our arguments: the kernel passes the parameters that it does not
    // know, e.g., "single".)
    if std::process::id() == 1 {
        if let Err(error) = boot() {
            log!("{error:#}");
            // PID 1 must not exit; stop the VM instead (midgard sees all vCPUs halt).
            sys::power_off();
        }
        return;
    }
    // The guest runs us (as HELPER) in other modes. A wrapper runs us as the user's login
    // shell (see launcher::helper), i.e., without arguments.
    let mode = std::env::args().nth(1);
    let result = match mode.as_deref() {
        None | Some("exec") => launcher::helper(),
        Some("ready") => ready::notify().map_err(|error| error.to_string()),
        Some("waypipe") => display::server(std::env::args().nth(2)),
        Some("pause") => display::pause(),
        Some(mode) => Err(format!("unknown mode {mode:?} (the agent runs as PID 1)")),
    };
    if let Err(error) = result {
        eprintln!("midgard-init: {error}");
        std::process::exit(1);
    }
}

/// Returns the value of `name=` on the kernel command line.
fn parameter(cmdline: &str, name: &str) -> Option<String> {
    cmdline
        .split_whitespace()
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

    let cmdline = std::fs::read_to_string("/proc/cmdline").context("reading /proc/cmdline")?;
    let root = parameter(&cmdline, param::ROOT)
        .with_context(|| format!("no {}= parameter", param::ROOT))?;
    let fstype = parameter(&cmdline, param::ROOT_FSTYPE).unwrap_or_else(|| "ext4".into());
    let config = load_config();
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
    install_helper();
    let config = CONFIG.get_or_init(|| config);
    user_setup(config);
    if let Some(waypipe) = &waypipe {
        display::install(waypipe, config);
    }
    let agent_display = waypipe.is_some() && config.display == Display::Agent;
    drop(waypipe);
    let watcher = ready::Watcher::new(config.ready);

    // With the distribution's init, it becomes PID 1 and we continue as its child. We
    // are still single-threaded here, which makes fork() safe.
    if let Some(init) = &config.init {
        // SAFETY: fork() in a single-threaded process.
        match unsafe { libc::fork() } {
            -1 => bail!("fork(): {}", std::io::Error::last_os_error()),
            0 => {
                INIT_RUNS.store(true, Ordering::Relaxed);
                // SAFETY: setsid() has no preconditions; it detaches us from init's
                // session and process group.
                unsafe { libc::setsid() };
                if let Err(error) = agent(watcher, agent_display) {
                    log!("{error:#}");
                }
                std::process::exit(1);
            }
            _ => exec_init(init),
        }
    }
    agent(watcher, agent_display)
}

/// Reads the guest configuration from the initramfs (before we leave it).
fn load_config() -> GuestConfig {
    let path = format!("/{CONFIG_FILE}");
    let result = File::open(&path)
        .and_then(|mut file| midgard_protocol::read_message(&mut file))
        .map(Option::unwrap_or_default);
    // The initramfs stays in memory.
    let _ = std::fs::remove_file(&path);
    result.unwrap_or_else(|error| {
        log!("reading {path}: {error}; using the default configuration");
        GuestConfig::default()
    })
}

/// Copies ourselves to [`HELPER`], such that the guest can run us.
fn install_helper() {
    let result = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(RUN_DIR)
        .and_then(|()| std::fs::copy("/proc/self/exe", HELPER))
        .and_then(|_| std::fs::set_permissions(HELPER, PermissionsExt::from_mode(0o755)));
    if let Err(error) = result {
        log!("installing {HELPER}: {error}");
    }
}

/// Runs the user setup command (see [`GuestConfig::user_setup`]).
fn user_setup(config: &GuestConfig) {
    let (Some(user), Some(setup)) = (&config.user, &config.user_setup) else {
        return;
    };
    let Some((program, args)) = setup.split_first() else {
        return;
    };
    log!("setting up user {user}");
    let status = std::process::Command::new(program)
        .args(args)
        .arg(user)
        .env_clear()
        .env("PATH", PATH)
        .env("HOME", "/root")
        .current_dir("/")
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => log!("{program} failed ({status})"),
        Err(error) => log!("running {program}: {error}"),
    }
}

/// Serves sessions (as PID 1 or as a child of the distribution's init) and runs the
/// Wayland display for the default user (with `agent_display`, see [`Display::Agent`]).
/// One thread runs everything (see [`rt`]).
fn agent(watcher: ready::Watcher, agent_display: bool) -> Result<()> {
    // Before any thread starts (async-io starts one).
    reaper::block_sigchld();
    rt::block_on(async {
        rt::spawn(reaper::run()).detach();
        // Sessions find the display running.
        let display = async move {
            if agent_display {
                let name = config().user.as_deref().unwrap_or("root");
                match users::lookup(name) {
                    Ok(user) => display::start(&user).await,
                    Err(error) => log!("{error}"),
                }
            }
        };
        rt::spawn(ready::watch(watcher, display)).detach();
        serve().await
    })
}

/// Replaces PID 1 with the distribution's init. If that fails, PID 1 only reaps orphans
/// (the agent already runs).
fn exec_init(init: &str) -> ! {
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
    loop {
        let mut status = 0;
        // SAFETY: waitpid() only writes the status.
        if unsafe { libc::waitpid(-1, &mut status, 0) } < 0 {
            std::thread::sleep(Duration::from_secs(1));
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

/// Accepts sessions on the agent port; each one is a task.
async fn serve() -> Result<()> {
    let listener = sys::vsock_listen(AGENT_PORT)?;
    // SAFETY: We own the new socket.
    let listener = Async::new(unsafe { OwnedFd::from_raw_fd(listener) })
        .context("registering the vsock listener")?;
    log!("ready");
    loop {
        match listener
            .read_with(|listener| sys::accept(listener.as_raw_fd()))
            .await
        {
            Ok(fd) => {
                // SAFETY: accept() returned a new socket that we own.
                let stream = unsafe { File::from_raw_fd(fd) };
                match Async::new(stream) {
                    Ok(stream) => rt::spawn(session::handle(stream)).detach(),
                    Err(error) => log!("vsock stream: {error}"),
                }
            }
            Err(error) => {
                log!("accept(): {error}");
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}
