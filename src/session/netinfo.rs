use std::os::fd::RawFd;
use std::time::Duration;

/// The kernel's smoothed round-trip time for a TCP socket. Costs one syscall and no traffic,
/// so it can be sampled every second without disturbing the session.
pub fn tcp_rtt(fd: RawFd) -> Option<Duration> {
    // SAFETY: `tcp_info` is plain old data, so all-zeroes is a valid value.
    let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
    let mut len = libc::socklen_t::try_from(size_of::<libc::tcp_info>()).ok()?;
    // SAFETY: `info` and `len` are valid for writes and `len` holds the buffer size. A stale
    // descriptor makes getsockopt fail with EBADF; it can't write out of bounds.
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            (&raw mut info).cast::<libc::c_void>(),
            &mut len,
        )
    };
    (ret == 0 && info.tcpi_rtt > 0).then(|| Duration::from_micros(u64::from(info.tcpi_rtt)))
}
