use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

static SIGNAL: AtomicI32 = AtomicI32::new(0);
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

#[cfg(target_os = "linux")]
unsafe fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno_ptr() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

extern "C" fn on_signal(signal: libc::c_int) {
    // Both atomics are lock-free integers. write(2) is async-signal-safe.
    unsafe {
        let errno = *errno_ptr();
        if signal != libc::SIGCHLD {
            let _ = SIGNAL.compare_exchange(0, signal, Ordering::Relaxed, Ordering::Relaxed);
        }
        let fd = WAKE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte = 1_u8;
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
        *errno_ptr() = errno;
    }
}

/// Signals wake poll immediately, including SIGCHLD for short commands.
pub struct Signals {
    pub reader: UnixStream,
    _writer: UnixStream,
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl Signals {
    pub fn install() -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        SIGNAL.store(0, Ordering::Relaxed);
        WAKE_FD.store(writer.as_raw_fd(), Ordering::Relaxed);
        let mut result = Self {
            reader,
            _writer: writer,
            previous: Vec::with_capacity(4),
        };
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGCHLD] {
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut previous = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as usize;
                action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, &mut previous) < 0 {
                    return Err(io::Error::last_os_error());
                }
                result.previous.push((signal, previous));
            }
        }
        Ok(result)
    }

    pub fn received(&self) -> Option<i32> {
        match SIGNAL.load(Ordering::Relaxed) {
            0 => None,
            signal => Some(signal),
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        WAKE_FD.store(-1, Ordering::Relaxed);
        for (signal, previous) in self.previous.iter().rev() {
            unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) };
        }
    }
}

pub fn signal_group(pid: u32, signal: i32, leader_exited: bool) -> io::Result<()> {
    // A positive child PID is also its process group ID (set when spawning).
    // The child remains unreaped until the last signal, preventing PID reuse.
    if unsafe { libc::kill(-(pid as libc::pid_t), signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH)
        // Darwin skips zombies when choosing signal recipients and reports
        // EPERM for a group containing only the retained zombie leader.
        // Never ignore permission failures while that leader is alive.
        || (cfg!(target_os = "macos")
            && leader_exited
            && error.raw_os_error() == Some(libc::EPERM))
    {
        Ok(())
    } else {
        Err(error)
    }
}

pub fn pending_output(fd: libc::c_int) -> io::Result<usize> {
    let mut bytes: libc::c_int = 0;
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut bytes) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(bytes.max(0) as usize)
    }
}

/// Observe an exit without reaping the process group leader. Keeping the zombie
/// reserves its PID until every group-directed cleanup signal has been sent.
pub fn child_exited(pid: u32) -> io::Result<bool> {
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let result = libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(info.si_signo == libc::SIGCHLD)
    }
}
