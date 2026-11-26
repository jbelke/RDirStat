//! The few libc calls the service needs that `std` does not offer. Each is a
//! plain syscall wrapper with no pointer kept past the call.

#![allow(unsafe_code, reason = "audited libc wrappers; see each block")]

use std::process::Command;

/// `gethostname(3)`, or `"unknown"`.
pub(crate) fn hostname() -> String {
    let mut buf = [0_u8; 256];
    // SAFETY: `buf` is valid for `buf.len()` bytes for the duration of the
    // call; gethostname writes at most that many and we NUL-terminate below.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) };
    if rc != 0 {
        return "unknown".to_owned();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Whether a process with this pid exists (`kill(pid, 0)`; `EPERM` means it
/// exists but belongs to someone else).
pub(crate) fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence and permission check.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// An id of the current boot: `kern.boottime` on macOS, the kernel's
/// `boot_id` on Linux.
pub(crate) fn boot_id() -> String {
    #[cfg(target_os = "macos")]
    {
        let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
        let mut len = std::mem::size_of::<libc::timeval>();
        let name = c"kern.boottime";
        // SAFETY: `name` is NUL-terminated; `tv` and `len` are valid for
        // writes and `len` holds `tv`'s size, which sysctl will not exceed.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&raw mut tv).cast(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            return format!("{}.{:06}", tv.tv_sec, tv.tv_usec);
        }
        "unknown".to_owned()
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_or_else(|_| "unknown".to_owned(), |id| id.trim().to_owned())
    }
}

/// Makes `command` start a new session (`setsid(2)`) in the child, so it
/// has no controlling terminal and outlives the shell or app that started it.
pub(crate) fn new_session(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: the closure runs in the forked child before exec and calls only
    // setsid(2), which is async-signal-safe; it allocates nothing.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
