//! How sessions run their commands as their users (see [`Launcher`]): the agent runs
//! them itself, systemd runs them in the user's systemd instance, or a command of the
//! guest (the wrapper) opens a session for the user and runs us in it as the helper (see
//! [`helper`]), which runs the command.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicU64, Ordering};

use midgard_protocol::guest::{LAUNCH_FD, Launch, Launcher};
use midgard_protocol::session::Exec;

use super::process::{self, Command, Credentials};
use super::sys::Error;
use super::users::{self, User};
use super::{HELPER, PATH, log};

/// A session's command, ready to start.
pub struct Prepared {
    pub command: Command,
    pub signals: Signals,
    /// Keeps [`Command::fd3`] open (until the session ends).
    _fd3: Option<OwnedFd>,
}

/// How signals get to a session's command.
pub enum Signals {
    /// To the process group that the session's terminal says, else to the session.
    Group,
    /// To the systemd unit that runs the command (with `systemctl --user kill`, with the
    /// environment and credentials of `systemd-run`).
    Unit {
        unit: String,
        env: Vec<String>,
        credentials: Option<Credentials>,
    },
}

impl Signals {
    /// Delivers `signal`; `group` is the session's process group (for [`Signals::Group`]).
    pub fn deliver(&self, group: i32, signal: i32) {
        match self {
            Signals::Group => {
                // SAFETY: kill() only takes numbers.
                unsafe { libc::kill(-group, signal) };
            }
            Signals::Unit {
                unit,
                env,
                credentials,
            } => {
                let Some(path) = process::resolve("systemctl", PATH) else {
                    log!("systemctl: command not found");
                    return;
                };
                let argv = [
                    "systemctl",
                    "--user",
                    "kill",
                    &format!("--signal={signal}"),
                    unit,
                ];
                let mut command = Command::new(path, argv.map(String::from).into(), env.clone());
                command.credentials = credentials.clone();
                if let Err(error) = process::spawn_background(&command) {
                    log!("{error}");
                }
            }
        }
    }
}

/// Prepares the command of `exec` for `user` (which starts in `cwd`).
pub fn prepare(
    launcher: &Launcher,
    exec: &Exec,
    user: &User,
    cwd: &str,
) -> Result<Prepared, Error> {
    match launcher {
        Launcher::Direct => direct(exec, user, cwd),
        Launcher::Systemd => systemd(exec, user, cwd),
        Launcher::Wrapper(wrapper) => wrapped(wrapper, exec, user, cwd),
    }
}

/// The variables that every process of the user gets.
fn user_environment(exec: &Exec, user: &User) -> Vec<(String, String)> {
    let mut env = vec![
        ("PATH".into(), PATH.into()),
        ("HOME".into(), user.home.clone()),
        ("USER".into(), user.name.clone()),
        ("LOGNAME".into(), user.name.clone()),
        ("SHELL".into(), user.shell.clone()),
    ];
    if exec.tty.is_some() {
        env.push(("TERM".into(), "linux".into()));
    }
    env
}

/// Adds `more` to `env` (replacing variables of the same name).
fn merge(env: &mut Vec<(String, String)>, more: &[(String, String)]) {
    for (name, value) in more {
        env.retain(|(n, _)| n != name);
        env.push((name.clone(), value.clone()));
    }
}

fn flatten(env: Vec<(String, String)>) -> Vec<String> {
    env.into_iter().map(|(n, v)| format!("{n}={v}")).collect()
}

fn credentials(user: &User) -> Option<Credentials> {
    (user.uid != 0).then(|| Credentials {
        uid: user.uid,
        gid: user.gid,
        groups: user.groups.clone(),
    })
}

/// The login shell's name as argv[0] (with a dash).
fn login_name(shell: &str) -> String {
    format!("-{}", shell.rsplit('/').next().unwrap_or("sh"))
}

fn command_not_found(program: &str) -> Error {
    Error::Message(format!("{program}: command not found"))
}

/// [`Launcher::Direct`]: we run the command with a minimal environment.
fn direct(exec: &Exec, user: &User, cwd: &str) -> Result<Prepared, Error> {
    let mut env = user_environment(exec, user);
    merge(&mut env, &exec.env);
    let path_env = env
        .iter()
        .find_map(|(name, value)| (name == "PATH").then_some(value.as_str()))
        .unwrap_or(PATH);
    let (path, argv) = if exec.argv.is_empty() {
        (user.shell.clone(), vec![login_name(&user.shell)])
    } else {
        let path = process::resolve(&exec.argv[0], path_env)
            .ok_or_else(|| command_not_found(&exec.argv[0]))?;
        (path, exec.argv.clone())
    };
    let mut command = Command::new(path, argv, flatten(env));
    command.cwd = cwd.into();
    // (Like the other launchers, if the user may not enter the directory.)
    command.fallback_cwd = Some(user.home.clone());
    command.credentials = credentials(user);
    Ok(Prepared {
        command,
        signals: Signals::Group,
        _fd3: None,
    })
}

/// Numbers the systemd units of sessions.
static NEXT_UNIT: AtomicU64 = AtomicU64::new(1);

/// [`Launcher::Systemd`]: `systemd-run --user` runs the command as a transient service
/// of the user's systemd instance (with the instance's environment).
fn systemd(exec: &Exec, user: &User, cwd: &str) -> Result<Prepared, Error> {
    let unit = format!(
        "midgard-session-{}.service",
        NEXT_UNIT.fetch_add(1, Ordering::Relaxed)
    );
    let path =
        process::resolve("systemd-run", PATH).ok_or_else(|| command_not_found("systemd-run"))?;
    let mut argv: Vec<String> = [
        "systemd-run",
        "--user",
        "--quiet",
        "--wait",
        "--collect",
        "--service-type=exec",
        if exec.tty.is_some() {
            "--pty"
        } else {
            "--pipe"
        },
    ]
    .map(String::from)
    .into();
    argv.push(format!("--unit={unit}"));
    // (With "-", systemd starts the command in the home directory if the user may not
    // enter the directory.)
    argv.push(format!("--property=WorkingDirectory=-{cwd}"));
    for (name, value) in &exec.env {
        argv.push(format!("--setenv={name}={value}"));
    }
    argv.push("--".into());
    if exec.argv.is_empty() {
        argv.extend([user.shell.clone(), "-l".into()]);
    } else {
        // systemd expands variables in the command ($$ is a dollar sign); older versions
        // cannot turn that off (--expand-environment=no).
        argv.extend(exec.argv.iter().map(|arg| arg.replace('$', "$$")));
    }
    // systemctl and systemd-run find the user's instance through XDG_RUNTIME_DIR.
    let mut env = user_environment(exec, user);
    env.push(("XDG_RUNTIME_DIR".into(), format!("/run/user/{}", user.uid)));
    let mut command = Command::new(path, argv, flatten(env));
    command.credentials = credentials(user);
    let signals = Signals::Unit {
        unit,
        env: command.env.clone(),
        credentials: command.credentials.clone(),
    };
    Ok(Prepared {
        command,
        signals,
        _fd3: None,
    })
}

/// [`Launcher::Wrapper`]: the wrapper runs the helper in a session of the user, which
/// gets the command from us (see [`Launch`]).
fn wrapped(wrapper: &[String], exec: &Exec, user: &User, cwd: &str) -> Result<Prepared, Error> {
    let argv: Vec<String> = wrapper
        .iter()
        .map(|arg| {
            arg.replace("{user}", &user.name)
                .replace("{helper}", HELPER)
        })
        .collect();
    let program = argv
        .first()
        .ok_or_else(|| Error::Message("the wrapper is empty".into()))?;
    let path = process::resolve(program, PATH).ok_or_else(|| command_not_found(program))?;
    let launch = Launch {
        argv: exec.argv.clone(),
        env: exec.env.clone(),
        cwd: cwd.into(),
    };
    let fd3 = request(&launch)?;
    let mut env = vec![("PATH".to_string(), PATH.to_string())];
    if exec.tty.is_some() {
        env.push(("TERM".into(), "linux".into()));
    }
    let mut command = Command::new(path, argv, flatten(env));
    command.fd3 = Some(fd3.as_raw_fd());
    Ok(Prepared {
        command,
        signals: Signals::Group,
        _fd3: Some(fd3),
    })
}

/// A file with the request for the helper (a memfd, which needs no reader while we
/// write it).
fn request(launch: &Launch) -> Result<OwnedFd, Error> {
    // SAFETY: memfd_create() takes a NUL-terminated name.
    let fd = unsafe { libc::memfd_create(c"midgard-launch".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(Error::last("memfd_create()"));
    }
    // SAFETY: We own the new file descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut frame = Vec::new();
    midgard_protocol::write_message(&mut frame, launch)
        .and_then(|()| file.write_all(&frame))
        .map_err(|error| Error::io("writing the launch request", error))?;
    // SAFETY: lseek() only takes numbers.
    if unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } != 0 {
        return Err(Error::last("lseek()"));
    }
    Ok(file.into())
}

/// `midgard-init` as the wrapper's helper (in the user's session): runs the command of
/// the request in [`LAUNCH_FD`]. Commands run through a POSIX login shell, such that the
/// profile (/etc/profile and ~/.profile) sets up the session's environment, as on a
/// login; the client's variables take precedence over the profile's.
pub fn helper() -> Result<(), String> {
    // SAFETY: The agent passes the request in this file descriptor, which we own now.
    let mut file = unsafe { File::from_raw_fd(LAUNCH_FD) };
    let launch: Launch = midgard_protocol::read_message(&mut file)
        .map_err(|error| format!("reading the launch request: {error}"))?
        .ok_or("no launch request")?;
    drop(file);
    // The wrapper may have set SHELL to us (e.g., runuser -s).
    // SAFETY: getuid() has no preconditions.
    let shell = users::shell_of(unsafe { libc::getuid() });
    let mut command = if launch.argv.is_empty() {
        let mut command = std::process::Command::new(&shell);
        command.arg0(login_name(&shell));
        command
    } else {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-l", "-c", "exec env -- \"$@\"", "sh"]);
        command.args(
            launch
                .env
                .iter()
                .map(|(name, value)| format!("{name}={value}")),
        );
        command.args(&launch.argv);
        command
    };
    command.env("SHELL", &shell).envs(launch.env);
    // The user may not be allowed into the directory; then the shell starts at home.
    if std::env::set_current_dir(&launch.cwd).is_err()
        && let Some(home) = std::env::var_os("HOME")
    {
        let _ = std::env::set_current_dir(home);
    }
    // (exec() restores SIGPIPE, which Rust ignores.)
    let error = command.exec();
    Err(format!("{:?}: {error}", command.get_program()))
}
