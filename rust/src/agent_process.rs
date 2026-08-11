use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::process::Stdio;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::agent_error::AgentStartupFailure;
use crate::agent_launch::ResolvedAgentCommand;
use crate::agent_profile::WorkspaceLeaseV1;
use crate::fork_fd_registry::{ForkExecGuard, ForkTracked};

const PROC_CHILDREN: &std::ffi::CStr = c"/proc/thread-self/children";

pub(crate) struct SpawnedAgent {
    pub(crate) guardian: Child,
    pub(crate) adapter_pid: u32,
    pub(crate) stdin: ForkTracked<ChildStdin>,
    pub(crate) stdout: ForkTracked<ChildStdout>,
    pub(crate) stderr: ForkTracked<ChildStderr>,
    pub(crate) shutdown: Option<ForkTracked<OwnedFd>>,
}

pub(crate) async fn terminate_and_reap(
    guardian: &mut Child,
    shutdown: &mut Option<ForkTracked<OwnedFd>>,
) {
    drop(shutdown.take());
    let _ = guardian.wait().await;
}

fn spawn_failure() -> AgentStartupFailure {
    AgentStartupFailure::start("spawn_failed", "spawn", "agent process could not start")
}

fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [0; 2];
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

unsafe fn close_range_except(retained: RawFd) {
    if let Ok(retained) = u32::try_from(retained) {
        let before_closed = retained <= 3
            || unsafe { libc::syscall(libc::SYS_close_range, 3_u32, retained - 1, 0_u32) } == 0;
        let after_closed = retained == u32::MAX
            || unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    retained.saturating_add(1),
                    u32::MAX,
                    0_u32,
                )
            } == 0;
        if before_closed && after_closed {
            return;
        }
    }

    let mut limit = MaybeUninit::<libc::rlimit>::uninit();
    let maximum = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } == 0 {
        unsafe { limit.assume_init() }.rlim_cur
    } else {
        65_536
    };
    let maximum = maximum.min(u64::from(u32::MAX));
    for descriptor in 3..maximum {
        let Ok(descriptor) = RawFd::try_from(descriptor) else {
            break;
        };
        if descriptor != retained {
            unsafe {
                libc::close(descriptor);
            }
        }
    }
}

unsafe fn kill_direct_children() {
    let descriptor =
        unsafe { libc::open(PROC_CHILDREN.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if descriptor < 0 {
        return;
    }
    let mut buffer = [0_u8; 4096];
    let mut pid = 0_i32;
    let mut has_digits = false;
    loop {
        let read = unsafe {
            libc::read(
                descriptor,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
            )
        };
        if read <= 0 {
            break;
        }
        for byte in &buffer[..usize::try_from(read).unwrap_or(0)] {
            if byte.is_ascii_digit() {
                has_digits = true;
                pid = pid
                    .saturating_mul(10)
                    .saturating_add(i32::from(*byte - b'0'));
            } else if has_digits {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                pid = 0;
                has_digits = false;
            }
        }
    }
    if has_digits {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    unsafe {
        libc::close(descriptor);
    }
}

unsafe fn cleanup_descendants(adapter_pid: libc::pid_t, mut adapter_status: Option<i32>) -> ! {
    if adapter_status.is_none() {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(adapter_pid, &mut status, libc::WNOHANG) };
        if waited == adapter_pid {
            adapter_status = Some(status);
        } else if waited == 0 {
            unsafe {
                libc::kill(-adapter_pid, libc::SIGKILL);
                libc::kill(adapter_pid, libc::SIGKILL);
            }
        }
    }

    loop {
        unsafe {
            kill_direct_children();
        }
        let mut status = 0;
        let waited = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if waited > 0 {
            if waited == adapter_pid {
                adapter_status = Some(status);
            }
            continue;
        }
        if waited == 0 {
            unsafe {
                libc::sched_yield();
            }
            continue;
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => break,
            _ => unsafe {
                libc::sched_yield();
            },
        }
    }

    let code = adapter_status.map_or(127, |status| {
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128_i32.saturating_add(libc::WTERMSIG(status)).min(255)
        } else {
            127
        }
    });
    unsafe {
        libc::_exit(code);
    }
}

unsafe fn reap_exited_children(adapter_pid: libc::pid_t) -> Option<i32> {
    loop {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if waited == adapter_pid {
            return Some(status);
        }
        if waited > 0 {
            continue;
        }
        if waited == 0 {
            return None;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return None;
        }
    }
}

unsafe fn guardian_main(
    adapter_pid: libc::pid_t,
    control: RawFd,
    signal_mask: &libc::sigset_t,
) -> ! {
    unsafe {
        libc::close(libc::STDIN_FILENO);
        libc::close(libc::STDOUT_FILENO);
        libc::close(libc::STDERR_FILENO);
        close_range_except(control);
    }
    let signal_fd =
        unsafe { libc::signalfd(-1, signal_mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
    if signal_fd < 0 {
        unsafe {
            cleanup_descendants(adapter_pid, None);
        }
    }

    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: control,
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
            libc::pollfd {
                fd: signal_fd,
                events: libc::POLLIN | libc::POLLERR,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
        if result < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            unsafe {
                cleanup_descendants(adapter_pid, None);
            }
        }
        if descriptors[0].revents != 0 {
            unsafe {
                cleanup_descendants(adapter_pid, None);
            }
        }
        if descriptors[1].revents != 0 {
            let mut info = MaybeUninit::<libc::signalfd_siginfo>::uninit();
            unsafe {
                libc::read(
                    signal_fd,
                    info.as_mut_ptr().cast::<libc::c_void>(),
                    size_of::<libc::signalfd_siginfo>(),
                );
            }
            if let Some(status) = unsafe { reap_exited_children(adapter_pid) } {
                unsafe {
                    cleanup_descendants(adapter_pid, Some(status));
                }
            }
        }
    }
}

fn read_adapter_pid(descriptor: RawFd) -> io::Result<u32> {
    let mut pid = 0_i32;
    let mut offset = 0_usize;
    while offset < size_of::<i32>() {
        let read = unsafe {
            libc::read(
                descriptor,
                (&mut pid as *mut i32).cast::<u8>().add(offset).cast(),
                size_of::<i32>() - offset,
            )
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "agent guardian did not publish its child PID",
            ));
        }
        offset += usize::try_from(read).unwrap_or(0);
    }
    u32::try_from(pid).map_err(|_| io::Error::other("agent guardian published an invalid PID"))
}

pub(crate) fn spawn_agent(
    command: &ResolvedAgentCommand,
    workspace: &WorkspaceLeaseV1,
) -> Result<SpawnedAgent, AgentStartupFailure> {
    let guard = ForkExecGuard::begin();
    let (control_read, control_write) = cloexec_pipe().map_err(|_| spawn_failure())?;
    let (pid_read, pid_write) = cloexec_pipe().map_err(|_| spawn_failure())?;

    let mut standard = std::process::Command::new(&command.program);
    standard
        .args(&command.args)
        .envs(command.environment.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in &command.removed_environment {
        standard.env_remove(name);
    }
    standard.process_group(0);
    let directory = workspace.directory.as_raw_fd();
    let control = control_read.as_raw_fd();
    let published_pid = pid_write.as_raw_fd();
    unsafe {
        standard.pre_exec(move || {
            if libc::fchdir(directory) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut signal_mask = MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(signal_mask.as_mut_ptr());
            libc::sigaddset(signal_mask.as_mut_ptr(), libc::SIGCHLD);
            let signal_mask = signal_mask.assume_init();
            let mut inherited_mask = MaybeUninit::<libc::sigset_t>::uninit();
            if libc::sigprocmask(libc::SIG_BLOCK, &signal_mask, inherited_mask.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let inherited_mask = inherited_mask.assume_init();
            let guardian_pid = libc::getpid();
            let adapter_pid = libc::fork();
            if adapter_pid < 0 {
                return Err(io::Error::last_os_error());
            }
            if adapter_pid == 0 {
                libc::close(control);
                libc::close(published_pid);
                libc::sigprocmask(libc::SIG_SETMASK, &inherited_mask, std::ptr::null_mut());
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != guardian_pid {
                    return Err(io::Error::other("agent guardian exited before exec"));
                }
                return Ok(());
            }

            let written = libc::write(
                published_pid,
                (&adapter_pid as *const libc::pid_t).cast::<libc::c_void>(),
                size_of::<libc::pid_t>(),
            );
            libc::close(published_pid);
            if written != isize::try_from(size_of::<libc::pid_t>()).unwrap_or(-1) {
                cleanup_descendants(adapter_pid, None);
            }
            guardian_main(adapter_pid, control, &signal_mask);
        });
    }

    let mut guardian = Command::from(standard)
        .spawn()
        .map_err(|_| spawn_failure())?;
    drop(control_read);
    drop(pid_write);
    let adapter_pid = read_adapter_pid(pid_read.as_raw_fd()).map_err(|_| spawn_failure())?;
    drop(pid_read);
    let stdin = guardian.stdin.take().expect("agent stdin was configured");
    let stdout = guardian.stdout.take().expect("agent stdout was configured");
    let stderr = guardian.stderr.take().expect("agent stderr was configured");
    let spawned = SpawnedAgent {
        guardian,
        adapter_pid,
        stdin: guard.track(stdin),
        stdout: guard.track(stdout),
        stderr: guard.track(stderr),
        shutdown: Some(guard.track(control_write)),
    };
    drop(guard);
    Ok(spawned)
}
