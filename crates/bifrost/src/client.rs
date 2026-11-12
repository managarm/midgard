//! Talking to midgard (on its socket, see [`midgard_protocol::control`]): runs commands
//! (or a login shell) in a VM instance, stops VMs, shows their console logs and manages
//! the VM instances. midgard starts an instance's VM for the first session with it; the
//! VM stays up for later commands.

use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use midgard_protocol::control::Listing;
use midgard_protocol::session::{Exec, Request, Response, WindowSize};
use midgard_protocol::size::format_size;
use midgard_protocol::{control, runtime};

use crate::cli::{CreateArgs, RunArgs};
use crate::{EXIT_FAILURE, log, terminal};

// The libc crate does not define these for managarm; mlibc uses the Linux ABI values.
const TIOCGWINSZ: libc::c_ulong = 0x5413;
const SA_RESTART: libc::c_int = 0x1000_0000;

/// Signals that we forward to the command.
const FORWARDED_SIGNALS: [i32; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty() only takes a file descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

fn window_size() -> Option<WindowSize> {
    // SAFETY: TIOCGWINSZ writes a struct winsize.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        (libc::ioctl(1, TIOCGWINSZ as _, &mut size) == 0).then_some(WindowSize {
            rows: size.ws_row,
            cols: size.ws_col,
        })
    }
}

/// Connects to midgard's socket (systemd starts midgard on the first connection).
pub fn connect_daemon() -> Result<UnixStream> {
    let socket = runtime::daemon_socket();
    UnixStream::connect(&socket).with_context(|| {
        format!(
            "failed to connect to midgard at {} (is midgard.socket running?)",
            socket.display()
        )
    })
}

/// Sends `request` to midgard and returns its answer.
fn ask(request: control::Request) -> Result<control::Response> {
    let mut stream = connect_daemon()?;
    midgard_protocol::write_message(&mut stream, &request)?;
    match midgard_protocol::read_message(&mut stream).context("lost the connection to midgard")? {
        Some(control::Response::Error(message)) => bail!("{message}"),
        Some(response) => Ok(response),
        None => bail!("midgard closed the connection"),
    }
}

/// The VM instances and the templates.
fn listing() -> Result<Listing> {
    match ask(control::Request::List)? {
        control::Response::List(listing) => Ok(listing),
        other => bail!("unexpected answer from midgard: {other:?}"),
    }
}

/// Formats rows as a table with aligned columns (the first row is the heading).
fn table<const N: usize>(rows: &[[String; N]]) -> String {
    let mut widths = [0; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (column, (cell, width)) in row.iter().zip(widths).enumerate() {
            if column + 1 < N {
                line += &format!("{cell:width$}  ");
            } else {
                line += cell;
            }
        }
        out += line.trim_end();
        out.push('\n');
    }
    out
}

/// Formats the VM instances and the templates for people.
fn format_listing(listing: &Listing) -> String {
    let mut out = String::new();
    if listing.instances.is_empty() {
        out += "No VM instances (bifrost create makes one).\n";
    } else {
        let mut rows = vec![["NAME", "TEMPLATE", "STATE", "DISK"].map(String::from)];
        for instance in &listing.instances {
            let state = if instance.error.is_some() {
                "invalid"
            } else if instance.running {
                "running"
            } else {
                "stopped"
            };
            rows.push([
                instance.name.clone(),
                instance.template.clone().unwrap_or_else(|| "-".into()),
                state.into(),
                instance.disk_size.map_or_else(|| "-".into(), format_size),
            ]);
        }
        out += &table(&rows);
    }
    out.push('\n');
    if listing.templates.is_empty() {
        out += &format!(
            "No templates in {} (guest/build.py install installs them).\n",
            listing.templates_dir.display()
        );
    } else {
        let mut rows = vec![["TEMPLATE", "DESCRIPTION"].map(String::from)];
        for template in &listing.templates {
            let description = match &template.error {
                Some(_) => "(invalid)".into(),
                None => template.description.clone().unwrap_or_default(),
            };
            rows.push([template.name.clone(), description]);
        }
        out += &table(&rows);
    }
    out
}

/// Lists the VM instances and the templates (and what is wrong with broken ones).
pub fn list() -> Result<i32> {
    let listing = listing()?;
    for (name, error) in listing
        .instances
        .iter()
        .map(|instance| (&instance.name, &instance.error))
        .chain(
            listing
                .templates
                .iter()
                .map(|template| (&template.name, &template.error)),
        )
    {
        if let Some(error) = error {
            log!("{name}: {error}");
        }
    }
    match io::stdout().write_all(format_listing(&listing).as_bytes()) {
        // E.g., `bifrost list | head`.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(0),
        result => result.map(|()| 0).map_err(Into::into),
    }
}

/// Asks a question on the terminal; returns the answer (without surrounding blanks).
fn prompt(question: &str) -> Result<String> {
    eprint!("{question}");
    io::stderr().flush()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 {
        bail!("no answer (end of input)");
    }
    Ok(answer.trim().to_string())
}

/// The user that sessions of a new VM instance run as: the answer to a question whose
/// default is our user name (which `yes`, or no terminal, takes without asking).
fn ask_user(yes: bool) -> Result<String> {
    let default = ["USER", "LOGNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|user| !user.is_empty()));
    if yes || !is_tty(0) {
        return default.context(
            "the template needs a user for the VM instance (bifrost create --user names it)",
        );
    }
    loop {
        let answer = match &default {
            Some(default) => prompt(&format!("User in the VM instance [{default}]: "))?,
            None => prompt("User in the VM instance: ")?,
        };
        match (answer.is_empty(), &default) {
            (false, _) => return Ok(answer),
            (true, Some(default)) => return Ok(default.clone()),
            (true, None) => {}
        }
    }
}

/// Creates the VM instance `name` from `template` (asking for the user if the template
/// needs one and `user` does not name it).
fn create_instance(
    name: &str,
    template: &str,
    user: Option<String>,
    memory: Option<u64>,
    disk_size: Option<u64>,
    yes: bool,
) -> Result<()> {
    let listing = listing()?;
    if listing
        .instances
        .iter()
        .any(|instance| instance.name == name)
    {
        bail!("the VM instance {name} exists already");
    }
    let Some(info) = listing.templates.iter().find(|info| info.name == template) else {
        let names: Vec<_> = listing.templates.iter().map(|info| &*info.name).collect();
        if names.is_empty() {
            bail!(
                "no template {template} (there are no templates in {}; guest/build.py \
                 install installs them)",
                listing.templates_dir.display()
            );
        }
        bail!("no template {template} (there are {})", names.join(", "));
    };
    if let Some(error) = &info.error {
        bail!("{error}");
    }
    let user = match user {
        None if info.needs_user => Some(ask_user(yes)?),
        user => user,
    };
    log!("creating {name} from the template {template}...");
    match ask(control::Request::Create {
        name: name.to_string(),
        template: template.to_string(),
        user,
        memory,
        disk_size,
    })? {
        control::Response::Done => Ok(()),
        other => bail!("unexpected answer from midgard: {other:?}"),
    }
}

/// Creates a VM instance (`bifrost create`).
pub fn create(args: &CreateArgs) -> Result<i32> {
    let template = args.template.as_deref().unwrap_or(&args.name);
    create_instance(
        &args.name,
        template,
        args.user.clone(),
        args.memory,
        args.disk_size,
        args.yes,
    )?;
    log!("created {}", args.name);
    Ok(0)
}

/// Deletes the VM instance `name` and its disk after asking (unless `yes`).
pub fn delete(name: &str, yes: bool) -> Result<i32> {
    let listing = listing()?;
    let Some(instance) = listing
        .instances
        .iter()
        .find(|instance| instance.name == name)
    else {
        bail!("no VM instance {name}");
    };
    if instance.running {
        bail!("{name} is running (bifrost shutdown {name} stops it)");
    }
    if !yes {
        if !is_tty(0) {
            bail!("not deleting {name} without --yes (stdin is not a terminal)");
        }
        let answer = prompt(&format!(
            "Delete the VM instance {name} and its disk? [y/N] "
        ))?;
        if !matches!(answer.as_str(), "y" | "Y" | "yes") {
            return Ok(EXIT_FAILURE.into());
        }
    }
    match ask(control::Request::Delete {
        name: name.to_string(),
    })? {
        control::Response::Done => {
            log!("deleted {name}");
            Ok(0)
        }
        other => bail!("unexpected answer from midgard: {other:?}"),
    }
}

/// Prints the console log of the VM of `name`; with `follow`, keeps printing what is
/// written to it (from the start again when its VM starts again).
pub fn log(name: &str, follow: bool) -> Result<i32> {
    let path = runtime::console_log(name);
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("{name} has no console log (it did not run since the host booted)")
        }
        Err(error) => return Err(error).with_context(|| format!("failed to open {path:?}")),
    };
    let mut stdout = io::stdout().lock();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut position = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count > 0 {
            position += count as u64;
            match stdout
                .write_all(&buffer[..count])
                .and_then(|()| stdout.flush())
            {
                Ok(()) => continue,
                // E.g., `bifrost log | head`.
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(0),
                Err(error) => return Err(error.into()),
            }
        }
        if !follow {
            return Ok(0);
        }
        // The VM's next start truncates the log.
        if file.metadata()?.len() < position {
            file.rewind()?;
            position = 0;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// midgard has no VM instance of this name.
#[derive(Debug)]
struct NoInstance(String);

impl std::fmt::Display for NoInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "no VM instance {} (bifrost create makes one)", self.0)
    }
}

impl std::error::Error for NoInstance {}

/// Connects to the agent in the VM of `name`, which midgard starts first if it does
/// not run (and `start`). Fails with [`NoInstance`] if there is no such instance.
fn connect(name: &str, start: bool) -> Result<UnixStream> {
    let mut stream = connect_daemon()?;
    let request = control::Request::Session {
        name: name.to_string(),
        start,
    };
    midgard_protocol::write_message(&mut stream, &request)?;
    loop {
        match midgard_protocol::read_message(&mut stream)
            .context("lost the connection to midgard")?
        {
            Some(control::Response::Starting) => {
                if is_tty(2) {
                    log!("starting {name}...");
                }
            }
            Some(control::Response::Ready) => return Ok(stream),
            Some(control::Response::NoInstance) => return Err(NoInstance(name.into()).into()),
            Some(control::Response::Error(message)) => bail!("{message}"),
            Some(other) => bail!("unexpected answer from midgard: {other:?}"),
            None => bail!("midgard closed the connection"),
        }
    }
}

// Signals reach us through a pipe (the handler only writes the signal number).
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: i32) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    let byte = signal as u8;
    // SAFETY: write() is async-signal-safe.
    unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
}

fn install_signal_handlers(tty: bool) -> Result<std::fs::File> {
    let mut fds = [0; 2];
    // SAFETY: pipe2() writes two file descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error()).context("pipe2() failed");
    }
    SIGNAL_PIPE.store(fds[1], Ordering::Relaxed);
    let mut signals = FORWARDED_SIGNALS.to_vec();
    if tty {
        signals.push(libc::SIGWINCH);
    }
    for signal in signals {
        // SAFETY: The handler only calls write().
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            action.sa_flags = SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    // SAFETY: We own the read end.
    Ok(unsafe { std::fs::File::from_raw_fd(fds[0]) })
}

/// Creates the missing VM instance `name` from the template of the same name, if there
/// is one, after asking (unless `yes`).
fn offer_instance(name: &str, yes: bool) -> Result<()> {
    let listing = listing()?;
    if !listing
        .templates
        .iter()
        .any(|template| template.name == name)
    {
        return Err(NoInstance(name.into()).into());
    }
    if !yes {
        if !is_tty(0) {
            bail!(
                "no VM instance {name} (bifrost create {name} creates it from the template \
                 {name})"
            );
        }
        let question = format!("No VM instance {name}. Create it from the template {name}? [Y/n] ");
        if !matches!(prompt(&question)?.as_str(), "" | "y" | "Y" | "yes") {
            return Err(NoInstance(name.into()).into());
        }
    }
    create_instance(name, name, None, None, None, yes)
}

/// Runs a command in the VM of `name` (which is created first if it is missing, see
/// [`offer_instance`]); returns its exit code.
pub fn run(name: &str, args: &RunArgs) -> Result<i32> {
    let stream = match connect(name, !args.no_start) {
        Err(error) if !args.no_start && error.is::<NoInstance>() => {
            offer_instance(name, args.yes)?;
            connect(name, true)?
        }
        result => result?,
    };
    let tty = args.tty().unwrap_or_else(|| is_tty(0) && is_tty(1));
    let mut env = Vec::new();
    if tty && let Ok(term) = std::env::var("TERM") {
        env.push(("TERM".to_string(), term));
    }
    env.extend(
        args.env()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    let exec = Exec {
        argv: args.command.clone(),
        env,
        cwd: args.cwd.clone(),
        // The agent starts the command there if the guest sees it.
        host_cwd: std::env::current_dir()
            .ok()
            .and_then(|dir| dir.to_str().map(str::to_string)),
        user: args.user.clone(),
        tty: if tty {
            Some(window_size().unwrap_or(WindowSize { rows: 24, cols: 80 }))
        } else {
            None
        },
    };
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    midgard_protocol::write_message(&mut *writer.lock().unwrap(), &Request::Exec(exec))?;
    if tty {
        terminal::enter_raw_mode(false);
    }
    let result = session(stream, writer, tty);
    terminal::restore();
    result
}

fn session(stream: UnixStream, writer: Arc<Mutex<UnixStream>>, tty: bool) -> Result<i32> {
    let send = |writer: &Arc<Mutex<UnixStream>>, request: Request| {
        let _ = midgard_protocol::write_message(&mut *writer.lock().unwrap(), &request);
    };

    // stdin -> the command. When the session ends, `quit` stops the thread before we
    // exit: managarm does not cancel a read or poll on the terminal that is still pending
    // when a process exits (the read takes the next input from the shell that started us,
    // and exiting with a pending poll can hang).
    let (quit_read, mut quit_write) = {
        let mut fds = [0; 2];
        // SAFETY: pipe2() writes two file descriptors.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error()).context("pipe2() failed");
        }
        // SAFETY: We own the new file descriptors.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    };
    let (stopped_sender, stopped) = std::sync::mpsc::channel::<()>();
    let input_writer = writer.clone();
    std::thread::spawn(move || {
        let _stopped = stopped_sender;
        let mut stdin = io::stdin().lock();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            // Read only once input is there (see above).
            let mut fds = [
                libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: quit_read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: poll() gets two valid pollfds.
            if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                send(&input_writer, Request::StdinEof);
                return;
            }
            if fds[1].revents != 0 {
                return;
            }
            match stdin.read(&mut buffer) {
                // Our signal handlers (e.g., SIGWINCH on a resize) interrupt the read:
                // managarm does not restart it despite SA_RESTART.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(0) | Err(_) => {
                    send(&input_writer, Request::StdinEof);
                    return;
                }
                Ok(count) => send(&input_writer, Request::Stdin(buffer[..count].to_vec())),
            }
        }
    });
    let mut stop_input = move || {
        let _ = quit_write.write_all(&[0]);
        let _ = stopped.recv_timeout(std::time::Duration::from_secs(1));
    };

    // Signals and window size changes -> the command.
    let mut signals = install_signal_handlers(tty)?;
    let signal_writer = writer.clone();
    std::thread::spawn(move || {
        let mut byte = [0u8];
        while signals.read_exact(&mut byte).is_ok() {
            let signal = byte[0] as i32;
            if signal == libc::SIGWINCH {
                if let Some(size) = window_size() {
                    send(&signal_writer, Request::Resize(size));
                }
            } else {
                send(&signal_writer, Request::Signal(signal));
            }
        }
    });

    // The command's output -> stdout / stderr.
    let result = (|| -> Result<i32> {
        let mut reader = stream;
        let mut stdout = io::stdout();
        let mut stderr = io::stderr();
        loop {
            match midgard_protocol::read_message::<Response>(&mut reader)
                .context("lost the connection to the VM")?
            {
                Some(Response::Started { .. }) => {}
                Some(Response::Stdout(data)) => {
                    stdout.write_all(&data)?;
                    stdout.flush()?;
                }
                Some(Response::Stderr(data)) => {
                    stderr.write_all(&data)?;
                    stderr.flush()?;
                }
                Some(Response::Exited { code, signal }) => {
                    return Ok(code.unwrap_or_else(|| 128 + signal.unwrap_or(0)));
                }
                Some(Response::Error(message)) => bail!("{message}"),
                Some(Response::Pong) => {}
                None => bail!("the VM closed the connection"),
            }
        }
    })();
    stop_input();
    result
}

/// The VM instances whose VMs run.
fn running() -> Result<Vec<String>> {
    match ask(control::Request::Running)? {
        control::Response::Running(names) => Ok(names),
        other => bail!("unexpected answer from midgard: {other:?}"),
    }
}

/// Stops all VMs that run.
pub fn shutdown_all() -> Result<()> {
    let mut failed = false;
    for name in running()? {
        if let Err(error) = shutdown(&name) {
            log!("{name}: {error:#}");
            failed = true;
        }
    }
    if failed {
        bail!("not all VMs stopped");
    }
    Ok(())
}

/// Stops the VM of a distribution.
pub fn shutdown(name: &str) -> Result<()> {
    let mut stream = connect(name, false)?;
    midgard_protocol::write_message(&mut stream, &Request::Shutdown)?;
    // The agent acknowledges before it powers off; then we wait until midgard says that
    // the VM stopped.
    match midgard_protocol::read_message::<Response>(&mut stream)
        .context("lost the connection to the VM")?
    {
        Some(Response::Exited { .. }) => {}
        Some(Response::Error(message)) => bail!("{message}"),
        other => bail!("unexpected answer to the shutdown request: {other:?}"),
    }
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(60);
    while running()?.iter().any(|running| running == name) {
        if Instant::now() > deadline {
            bail!("{name} did not stop within 60 s");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use midgard_protocol::control::{InstanceInfo, TemplateInfo};

    #[test]
    fn listing() {
        let listing = Listing {
            instances: vec![
                InstanceInfo {
                    name: "alpine".into(),
                    template: Some("alpine".into()),
                    running: true,
                    disk_size: Some(3 << 29),
                    error: None,
                },
                InstanceInfo {
                    name: "broken".into(),
                    template: None,
                    running: false,
                    disk_size: None,
                    error: Some("invalid instance.toml".into()),
                },
            ],
            templates: vec![TemplateInfo {
                name: "debian".into(),
                description: Some("Debian".into()),
                needs_user: true,
                error: None,
            }],
            templates_dir: "/templates".into(),
        };
        assert_eq!(
            format_listing(&listing),
            "NAME    TEMPLATE  STATE    DISK\n\
             alpine  alpine    running  1.5 GiB\n\
             broken  -         invalid  -\n\
             \n\
             TEMPLATE  DESCRIPTION\n\
             debian    Debian\n"
        );
        let empty = Listing {
            instances: Vec::new(),
            templates: Vec::new(),
            templates_dir: "/templates".into(),
        };
        assert_eq!(
            format_listing(&empty),
            "No VM instances (bifrost create makes one).\n\n\
             No templates in /templates (guest/build.py install installs them).\n"
        );
    }
}
