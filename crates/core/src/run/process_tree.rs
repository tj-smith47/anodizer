//! Per-platform child-subtree lifetime management for the `run_*` exec API:
//! the reap primitive (Unix process group / Windows Job Object), the registry
//! of live timeout-bounded subtrees, and the external-termination handler that
//! propagates a SIGTERM/SIGINT to every one of them before anodizer exits.
//!
//! Kept apart from [`super::exec`] because none of it is on the capture path:
//! the exec loop only *registers* a spawned tree and asks for a reap, while
//! everything about HOW a subtree is isolated and killed — the `libc::kill`
//! process-group form, the hand-rolled Job Object FFI, the self-pipe signal
//! handler — lives here.

#[cfg(windows)]
use std::process::Stdio;
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};

/// Place a to-be-spawned, timeout-bounded child in its OWN process group so the
/// watchdog can kill the WHOLE subtree on expiry — not just the immediate
/// child.
///
/// A bare `Child::kill()` reaps only the direct child; a child that forked a
/// grandchild holding the inherited stdout/stderr pipe (e.g. a `sh -c` wrapper
/// around the real tool, or a relay that double-forks) would keep those pipes
/// open after the parent died, so the reader threads never hit EOF and the run
/// would hang until the grandchild exited on its own. Killing the process group
/// closes every inherited pipe at once. Applied ONLY on the timeout path so the
/// untimed `Command` setup is byte-for-byte unchanged.
#[cfg(unix)]
pub(super) fn set_own_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // 0 → put the child in a new group whose pgid equals its pid.
    cmd.process_group(0);
}

#[cfg(windows)]
pub(super) fn set_own_process_group(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    // CREATE_NEW_PROCESS_GROUP isolates the child from console control events
    // aimed at our own group (a stray Ctrl-C won't race the watchdog). The
    // subtree reap itself is done by a Job Object (`TerminateJobObject` in
    // `ChildTree::reap`) — unlike a Unix process group, a Windows process group
    // is NOT a kill target for TerminateProcess, and `taskkill /T` cannot reach
    // a subtree whose root has already exited (the post-exit drain case).
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
pub(super) fn set_own_process_group(_cmd: &mut Command) {}

/// Per-platform handle that reaps a whole timeout-bounded child subtree on
/// demand — crucially, **independent of whether the direct child is still
/// alive**, since the post-exit drain reap fires only AFTER the child has exited
/// while a leaked grandchild keeps the inherited pipe open.
///
/// - **Unix**: the child's pgid (== pid, set at spawn via
///   [`set_own_process_group`]); reaped via `kill(-pgid, signal)`. The group
///   outlives its leader, so a leaked descendant is reaped after the leader
///   exits.
/// - **Windows**: the child's pid (registry key + `taskkill` fallback target)
///   plus an optional [`JobHandle`](windows_job::JobHandle) for the Job Object
///   the child and every process it spawns belong to; reaped via
///   `TerminateJobObject`. Job membership — not a live root — anchors the tree,
///   so descendants are reaped after the direct child exits. `taskkill /T`
///   cannot serve that case: it walks from a LIVE root present in a process
///   snapshot, and a terminated child is absent from that snapshot, so its
///   orphans survive (the bug the Job Object replaces).
///
/// `Copy` so it lives in the static registry, threads into the scoped watchdog,
/// and reaps from either site without ownership juggling.
#[derive(Clone, Copy)]
pub(super) struct ChildTree {
    /// Unix pgid (== child pid); Windows child pid.
    pub(super) pid: i32,
    /// Windows: the kill-on-close Job Object enclosing the child + descendants.
    /// `None` when the child could not be assigned to a job (a rare pre-Win8
    /// nested-job restriction) — the reap then falls back to `taskkill /T`.
    #[cfg(windows)]
    pub(super) job: Option<windows_job::JobHandle>,
}

impl ChildTree {
    /// Reap the whole subtree, best-effort (an already-reaped subtree yields a
    /// benign error). `signal` selects the Unix disposition — the timeout
    /// watchdog passes `SIGKILL` (unconditional), the external-termination
    /// watcher passes `SIGTERM` (let a well-behaved child clean up first); it is
    /// ignored on Windows, which has no graceful disposition for an opaque
    /// subtree.
    fn reap(self, signal: i32) {
        #[cfg(unix)]
        {
            // Negative pid targets the process GROUP. SAFETY: `kill(2)` with a
            // negative pid is async-signal-safe, has no memory effects, and an
            // already-reaped group yields ESRCH (ignored).
            unsafe {
                libc::kill(-self.pid, signal);
            }
        }
        #[cfg(windows)]
        {
            let _ = signal; // no graceful disposition for an opaque subtree
            match self.job {
                // Fast, non-blocking syscall; reaps every job member regardless
                // of the direct child's liveness — the drain-reap case.
                Some(job) => job.terminate(),
                // No job (assignment failed): fall back to the `taskkill /T`
                // walk, which still reaps a LIVE root's descendants.
                None => taskkill_tree(self.pid),
            }
        }
    }
}

/// Best-effort `taskkill /T /F /PID <pid>` — the Windows fallback used ONLY when
/// a child could not be enclosed in a Job Object. Walks the process tree from a
/// LIVE root (a terminated root is absent from the snapshot, so this cannot reap
/// a drain-orphaned grandchild — that is the Job Object's role). Resolved by
/// absolute System32 path so a sanitized PATH can't strip the tool. NOT
/// signal-safe (spawns a subprocess); only a normal watcher thread calls it.
#[cfg(windows)]
fn taskkill_tree(pid: i32) {
    let taskkill = std::env::var_os("SystemRoot")
        .map(|root| {
            std::path::Path::new(&root)
                .join("System32")
                .join("taskkill.exe")
        })
        .unwrap_or_else(|| std::path::PathBuf::from("taskkill.exe"));
    let _ = std::process::Command::new(taskkill)
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Reap `child` and its whole subtree (via [`ChildTree::reap`]), then the direct
/// child as a portable fallback. The timeout path is unconditional, so Unix uses
/// `SIGKILL`. Best-effort: a child that already exited yields a benign error.
pub(super) fn kill_child_tree(child: &mut Child, tree: ChildTree) {
    #[cfg(unix)]
    tree.reap(libc::SIGKILL);
    #[cfg(windows)]
    tree.reap(0);
    // Portable fallback: still reap the immediate child when the subtree reap
    // above was a no-op or unavailable.
    let _ = child.kill();
}

/// Windows Job Object FFI: encloses a timeout-bounded child (and every process
/// it spawns) so the watchdog can reap the WHOLE subtree via `TerminateJobObject`
/// even after the direct child has exited — the drain-reap case `taskkill /T`
/// cannot serve. Hand-rolled `extern "system"` declarations (mirroring the
/// `SetConsoleCtrlHandler` FFI in [`windows_termination`]) keep the heavyweight
/// `windows` crate out of the determinism-sensitive build.
#[cfg(windows)]
pub(super) mod windows_job {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle as _;
    use std::process::Child;

    type Handle = *mut c_void;
    type Bool = i32;
    type Dword = u32;

    /// `JOBOBJECTINFOCLASS::JobObjectExtendedLimitInformation`.
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: Dword = 0x0000_2000;
    const JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION: Dword = 0x0000_0400;

    // The three structs mirror the Win32 `JOBOBJECT_*` layouts exactly so the
    // pointer handed to `SetInformationJobObject` has the right size/offsets;
    // only `limit_flags` is read back, so the rest are layout-only fields.
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: Dword,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: Dword,
        affinity: usize,
        priority_class: Dword,
        scheduling_class: Dword,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    unsafe extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(
            job: Handle,
            class: i32,
            info: *const c_void,
            len: Dword,
        ) -> Bool;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
        fn TerminateJobObject(job: Handle, exit_code: Dword) -> Bool;
        fn CloseHandle(object: Handle) -> Bool;
    }

    /// A Job Object handle, stored as `isize` so it is `Send`/`Sync` for the
    /// static registry and the scoped watchdog. (A raw `HANDLE` pointer is
    /// neither, but the value is an opaque kernel handle — safe to move/share;
    /// the Win32 calls that consume it are themselves thread-safe.)
    #[derive(Clone, Copy)]
    pub struct JobHandle(isize);
    // SAFETY: an opaque kernel handle is just an integer the OS interprets; the
    // Job Object Win32 APIs accept it from any thread.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl JobHandle {
        /// Reap every process still in the job — including descendants orphaned
        /// by the direct child's exit. Best-effort: an already-terminated/closed
        /// job yields a benign failure.
        pub fn terminate(self) {
            // SAFETY: `TerminateJobObject` on a job handle we created; a failure
            // (job already gone) is ignored.
            unsafe {
                let _ = TerminateJobObject(self.0 as Handle, 1);
            }
        }

        /// Close the job handle on teardown. With `KILL_ON_JOB_CLOSE` the final
        /// handle close reaps any straggler still in the job (the last
        /// leak-prevention net). Paired 1:1 with [`enclose_child`].
        pub fn close(self) {
            // SAFETY: closing a handle we own exactly once.
            unsafe {
                let _ = CloseHandle(self.0 as Handle);
            }
        }
    }

    /// Create a kill-on-close Job Object and assign `child` (and, implicitly,
    /// every process it later spawns) to it, returning the job handle.
    ///
    /// Returns `None` if any step fails — notably a pre-Win8 nested-job
    /// restriction blocking assignment; the caller then falls back to the
    /// `taskkill /T` walk (which still reaps a LIVE root's descendants).
    ///
    /// The assignment races a grandchild the child might fork in the microseconds
    /// between spawn and assignment: such a grandchild escapes the job. The
    /// window is negligible in practice — the bounded tools (snapcraft, docker,
    /// `git push`) do real work before forking — and the `taskkill` fallback
    /// covers a missing job.
    pub fn enclose_child(child: &Child) -> Option<JobHandle> {
        // SAFETY: each call uses a job handle we just created plus the child's
        // own process handle; every failure is checked and unwinds via
        // `CloseHandle` so no handle leaks.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info: JobObjectExtendedLimitInformation = std::mem::zeroed();
            info.basic_limit_information.limit_flags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
            if SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                std::ptr::addr_of!(info) as *const c_void,
                std::mem::size_of::<JobObjectExtendedLimitInformation>() as Dword,
            ) == 0
            {
                let _ = CloseHandle(job);
                return None;
            }
            if AssignProcessToJobObject(job, child.as_raw_handle() as Handle) == 0 {
                let _ = CloseHandle(job);
                return None;
            }
            Some(JobHandle(job as isize))
        }
    }
}

/// Process-global registry of live, timeout-bounded child subtrees, keyed by the
/// child's pid (Unix: == pgid; Windows: the Job Object owner). Populated only for
/// the timeout-bounded path — the long-running snapcraft / docker / git subtrees
/// that survive a cancel — so the external-termination watcher can reap every one
/// before anodizer itself dies.
///
/// A plain `Mutex` is safe here because it is locked ONLY from normal threads
/// (`capture_inner` on spawn/reap, the watcher thread on signal) — never from
/// the async-signal-safe handler, which touches only the self-pipe.
static LIVE_CHILD_TREES: OnceLock<Mutex<std::collections::HashMap<i32, ChildTree>>> =
    OnceLock::new();

pub(super) fn live_child_trees() -> &'static Mutex<std::collections::HashMap<i32, ChildTree>> {
    LIVE_CHILD_TREES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Record a spawned, timeout-bounded child tree so the external-termination
/// watcher can reach it. Paired with [`deregister_child_tree`] on reap.
pub(super) fn register_child_tree(tree: ChildTree) {
    live_child_trees()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(tree.pid, tree);
}

/// Drop a reaped child tree from the registry so a recycled pid is never reaped
/// by a later termination.
pub(super) fn deregister_child_tree(pid: i32) {
    live_child_trees()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&pid);
}

/// RAII guard that deregisters a registered child tree on every exit edge of
/// `capture_inner` — the pipe-take `?`s, the watchdog/stdin error returns,
/// the success return, and an unwinding panic. A manual deregister could only
/// cover the edges before it and would leak the pid past any earlier `?` or
/// `thread::scope` panic, after which an OS pid-recycle would let an external
/// termination reap an unrelated subtree.
///
/// On Windows it also closes the Job Object handle, which (with
/// `KILL_ON_JOB_CLOSE`) reaps any straggler still in the job. It runs AFTER the
/// `thread::scope` joins, so the watchdog can never touch a closed handle.
pub(super) struct TreeRegistration(pub(super) ChildTree);

impl Drop for TreeRegistration {
    fn drop(&mut self) {
        deregister_child_tree(self.0.pid);
        #[cfg(windows)]
        if let Some(job) = self.0.job {
            job.close();
        }
    }
}

/// Reap every registered child subtree. Run by the watcher thread (NOT a signal
/// handler), so locking the registry and issuing the kills is safe. Returns the
/// number of trees reaped. Unix uses `SIGTERM` (a well-behaved child cleans up
/// before anodizer re-raises and dies); Windows uses `TerminateJobObject` (no
/// graceful disposition for an opaque subtree).
pub(super) fn terminate_all_child_trees() -> usize {
    let trees: Vec<ChildTree> = {
        let guard = live_child_trees().lock().unwrap_or_else(|p| p.into_inner());
        guard.values().copied().collect()
    };
    for tree in trees.iter().copied() {
        #[cfg(unix)]
        tree.reap(libc::SIGTERM);
        #[cfg(windows)]
        tree.reap(0);
    }
    trees.len()
}

/// Install a one-shot handler so an EXTERNAL SIGTERM/SIGINT (a GitHub Actions
/// job cancel, a runner job-timeout, an operator `Ctrl-C`) propagates to every
/// group-isolated child subtree before anodizer exits — instead of orphaning a
/// hung snapcraft/docker subtree that then holds the CI runner open long after
/// anodizer is gone.
///
/// Idempotent and infallible from the caller's view: call once, early, before
/// the pipeline runs. A second call (or a platform without the primitive) is a
/// silent no-op. On the unsupported-platform fallback the process keeps its
/// default signal disposition (terminate), so behavior is unchanged there.
///
/// # Mechanism (async-signal-safety)
///
/// Unix uses the classic **self-pipe**: the installed `sigaction` handler does
/// nothing but `write(2)` a single byte to a pipe — the one syscall guaranteed
/// async-signal-safe — and a normal watcher thread blocked on `read(2)` does
/// the actual work (lock the registry, group-`SIGTERM` each child tree, then
/// reset the signal to its default disposition and re-raise so anodizer dies
/// WITH the right signal exit code, AFTER its children got the signal). The
/// handler never locks, allocates, or logs.
pub fn install_termination_handler() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return; // already installed
    }
    #[cfg(unix)]
    unix_termination::install();
    #[cfg(windows)]
    windows_termination::install();
}

#[cfg(unix)]
mod unix_termination {
    use super::terminate_all_child_trees;
    use std::os::unix::io::RawFd;
    use std::sync::atomic::{AtomicI32, Ordering};

    /// Write end of the self-pipe, set BEFORE the handler is armed so a signal
    /// can never observe an uninitialized fd. The handler reads it relaxed and
    /// writes one byte; that is the only work it does.
    static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    /// Carries which signal fired from the handler to the watcher (for the
    /// re-raise), so anodizer exits with the same signal that hit it.
    static FIRED_SIGNAL: AtomicI32 = AtomicI32::new(0);

    /// The `sigaction` handler: async-signal-safe by construction — it records
    /// the signal number and writes ONE byte to the self-pipe, nothing else.
    /// No lock, no allocation, no logging.
    extern "C" fn on_signal(sig: libc::c_int) {
        FIRED_SIGNAL.store(sig, Ordering::SeqCst);
        let fd = WAKE_WRITE_FD.load(Ordering::SeqCst);
        if fd >= 0 {
            let byte: u8 = 1;
            // SAFETY: `write(2)` is async-signal-safe; a single-byte write to a
            // valid pipe fd has no memory effects. A short/failed write (EINTR,
            // full pipe) is ignored — one queued byte already wakes the watcher.
            unsafe {
                let _ = libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
            }
        }
    }

    pub fn install() {
        let mut fds: [RawFd; 2] = [-1, -1];
        // SAFETY: `pipe(2)` fills the two-element array with valid fds or
        // returns non-zero; on failure the handler is never armed.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return;
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // Publish the write fd BEFORE arming the handler so no early signal can
        // race a -1 fd.
        WAKE_WRITE_FD.store(write_fd, Ordering::SeqCst);

        // SAFETY: zeroed `sigaction` is a valid empty struct; we then set the
        // handler and an empty mask. `sigaction(2)` itself is the documented
        // installation API.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        }

        std::thread::Builder::new()
            .name("anodizer-sigwatch".into())
            .spawn(move || watcher(read_fd))
            .ok();
    }

    /// Normal watcher thread: blocks on the self-pipe, then group-`SIGTERM`s
    /// every live child tree and re-raises the original signal so anodizer dies
    /// WITH its children (correct signal exit code), not before them.
    fn watcher(read_fd: RawFd) -> ! {
        let mut byte = [0u8; 1];
        // SAFETY: a blocking `read(2)` of one byte from the read end of our own
        // pipe; the buffer outlives the call. EINTR is treated as "woken".
        loop {
            let n = unsafe { libc::read(read_fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
            if n != 0 {
                break; // a byte (signal) arrived, or EINTR — either way, act
            }
        }

        terminate_all_child_trees();

        let sig = FIRED_SIGNAL.load(Ordering::SeqCst);
        let sig = if sig == 0 { libc::SIGTERM } else { sig };
        // Reset to default disposition and re-raise so the process terminates
        // with the SAME signal that hit it (right exit code for CI), now that
        // its children already received SIGTERM.
        // SAFETY: restoring SIG_DFL and `raise`ing are async-signal-safe and
        // have no memory effects.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            libc::sigaction(sig, &sa, std::ptr::null_mut());
            libc::raise(sig);
        }
        // `raise` of a default-disposition terminating signal does not return;
        // the explicit exit is an unreachable belt-and-suspenders.
        std::process::exit(128 + sig);
    }
}

#[cfg(windows)]
mod windows_termination {
    use super::terminate_all_child_trees;
    use std::sync::atomic::{AtomicBool, Ordering};

    type Bool = i32;
    type Dword = u32;

    const TRUE: Bool = 1;
    const CTRL_C_EVENT: Dword = 0;
    const CTRL_BREAK_EVENT: Dword = 1;
    const CTRL_CLOSE_EVENT: Dword = 2;
    const CTRL_LOGOFF_EVENT: Dword = 5;
    const CTRL_SHUTDOWN_EVENT: Dword = 6;

    static FIRED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: Bool) -> Bool;
    }

    type HandlerRoutine = unsafe extern "system" fn(ctrl_type: Dword) -> Bool;

    /// Console control handler: Windows runs it on a dedicated thread (NOT a
    /// Unix-style async-signal context), so locking the registry and spawning
    /// `taskkill /T /F` from here is safe. Kills every live child tree, then
    /// returns FALSE so the default handler runs and terminates anodizer —
    /// children gone first, anodizer second.
    unsafe extern "system" fn on_ctrl(ctrl_type: Dword) -> Bool {
        match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT => {
                FIRED.store(true, Ordering::SeqCst);
                terminate_all_child_trees();
                // FALSE → fall through to the default handler, which terminates
                // the process now that its child trees are killed.
                0
            }
            _ => 0,
        }
    }

    pub fn install() {
        // SAFETY: registering a console control handler; the function pointer
        // is a valid `extern "system"` routine for the lifetime of the process.
        unsafe {
            SetConsoleCtrlHandler(Some(on_ctrl), TRUE);
        }
    }
}
