//! Waiting on a child's pipes without blocking in `read` or `write`, so a
//! reader or the stdin writer that the exec loop has abandoned notices it
//! within [`PIPE_POLL_INTERVAL`] and closes its end of the pipe.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Bytes the Windows stdin writer hands the pipe per write, so it can notice
/// `abandon` between writes of a payload larger than the pipe buffer. The Unix
/// writer needs no chunking: its pipe is non-blocking, so a write never takes
/// more than the pipe will accept.
#[cfg(windows)]
pub(super) const STDIN_CHUNK: usize = 16 * 1024;

/// The longest a reader or the stdin writer goes without checking `abandon`
/// while its pipe is not ready.
pub(super) const PIPE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A pipe end whose OS handle the readiness wait can ask about.
#[cfg(unix)]
pub(super) trait RawPipe: std::os::fd::AsRawFd {}
#[cfg(unix)]
impl<T: std::os::fd::AsRawFd> RawPipe for T {}
/// A pipe end whose OS handle the readiness wait can ask about.
#[cfg(windows)]
pub(super) trait RawPipe: std::os::windows::io::AsRawHandle {}
#[cfg(windows)]
impl<T: std::os::windows::io::AsRawHandle> RawPipe for T {}

/// The error a pipe wait returns once `abandon` is set; the thread then drops
/// its pipe end and returns.
fn abandoned() -> std::io::Error {
    std::io::Error::other("pipe abandoned after the post-exit drain window")
}

/// A child's output pipe that is read only once it has data or has closed, so
/// the reader checks `abandon` at least every [`PIPE_POLL_INTERVAL`] instead
/// of blocking in `read` until a leftover process writes again.
pub(super) struct AbandonableReader<R> {
    pub(super) pipe: R,
    pub(super) abandon: Arc<AtomicBool>,
}

impl<R: Read + RawPipe> Read for AbandonableReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        wait_readable(&self.pipe, &self.abandon)?;
        self.pipe.read(buf)
    }
}

/// Wait until `fd` reports one of `events` (or a hang-up or error, which the
/// following read or write reports), checking `abandon` between polls.
#[cfg(unix)]
fn wait_ready(
    fd: std::os::fd::RawFd,
    events: libc::c_short,
    abandon: &AtomicBool,
) -> std::io::Result<()> {
    let timeout_ms = PIPE_POLL_INTERVAL.as_millis() as libc::c_int;
    loop {
        if abandon.load(Ordering::Acquire) {
            return Err(abandoned());
        }
        let mut pfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: `pfd` is one valid, initialised `pollfd` that outlives the
        // call, and `fd` is owned by the pipe the caller holds.
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ready > 0 {
            return Ok(());
        }
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

#[cfg(unix)]
fn wait_readable(pipe: &impl RawPipe, abandon: &AtomicBool) -> std::io::Result<()> {
    wait_ready(pipe.as_raw_fd(), libc::POLLIN, abandon)
}

/// Wait until the pipe holds data, or `PeekNamedPipe` fails (the write end
/// closed, which the following read reports as EOF), checking `abandon`
/// between peeks. The sleep between empty peeks starts at 1 ms and doubles up
/// to [`PIPE_POLL_INTERVAL`]: a fixed 100 ms sleep would cap a busy child at
/// one pipe buffer per sleep, since the child blocks once the buffer is full.
#[cfg(windows)]
fn wait_readable(pipe: &impl RawPipe, abandon: &AtomicBool) -> std::io::Result<()> {
    let handle = pipe.as_raw_handle();
    let mut idle = Duration::from_millis(1);
    loop {
        if abandon.load(Ordering::Acquire) {
            return Err(abandoned());
        }
        let mut available: u32 = 0;
        // SAFETY: `handle` is the read end of an anonymous pipe owned by the
        // caller; the buffer pointers are null with a zero length, which the
        // API accepts when only the available byte count is asked for.
        let ok = unsafe {
            windows_pipe::PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || available > 0 {
            return Ok(());
        }
        std::thread::sleep(idle);
        idle = (idle * 2).min(PIPE_POLL_INTERVAL);
    }
}

/// `PeekNamedPipe`, declared by hand like the Job Object calls in
/// `super::process_tree::windows_job` so the `windows` crate stays out of the build.
#[cfg(windows)]
mod windows_pipe {
    use std::ffi::c_void;

    unsafe extern "system" {
        pub(super) fn PeekNamedPipe(
            pipe: *mut c_void,
            buffer: *mut c_void,
            buffer_size: u32,
            bytes_read: *mut u32,
            total_bytes_available: *mut u32,
            bytes_left_this_message: *mut u32,
        ) -> i32;
    }
}

/// Write `bytes` to the child's stdin, stopping with [`abandoned`] once
/// `abandon` is set. The pipe is switched to non-blocking mode and written
/// only when `poll` says it has room, so the thread never waits in `write`.
#[cfg(unix)]
pub(super) fn write_payload(
    pipe: &mut std::process::ChildStdin,
    bytes: &[u8],
    abandon: &AtomicBool,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let fd = pipe.as_raw_fd();
    // SAFETY: `fd` is the parent's write end, owned by `pipe` for the whole
    // call; F_GETFL / F_SETFL only change its status flags.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let mut rest = bytes;
    while !rest.is_empty() {
        wait_ready(fd, libc::POLLOUT, abandon)?;
        match pipe.write(rest) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => rest = &rest[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Write `bytes` to the child's stdin in [`STDIN_CHUNK`] pieces, stopping with
/// [`abandoned`] once `abandon` is set. A write into a pipe nobody reads still
/// blocks; see `POST_EXIT_DRAIN_WINDOW`.
#[cfg(windows)]
pub(super) fn write_payload(
    pipe: &mut std::process::ChildStdin,
    bytes: &[u8],
    abandon: &AtomicBool,
) -> std::io::Result<()> {
    for chunk in bytes.chunks(STDIN_CHUNK) {
        if abandon.load(Ordering::Acquire) {
            return Err(abandoned());
        }
        pipe.write_all(chunk)?;
    }
    Ok(())
}
