//! This binds the fuse file descriptor to the tokio reactor.

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// This is originally meant to simply be wrapped in tokio's `AsyncFd`, however, it doesn't
/// provide a way to wait catch `POLLERR` explicitly (or at all when just waiting for read
/// events).
/// This means we need a custom polling mechanism unfortunately.
pub struct FuseFd {
    fd: RawFd,
    state: Arc<PollState>,
    thread: Option<std::thread::JoinHandle<()>>,
    poll: Arc<Epoll>,
}

impl Drop for FuseFd {
    fn drop(&mut self) {
        let _ = self.poll.signal_finish();
        let _ = self.thread.take().unwrap().join();
    }
}

impl FuseFd {
    /// Create a `FuseFD` handle from a raw file descriptor.
    ///
    /// The file descriptor will not be closed when this instance is dropped, it is considered to
    /// be "borrowing" from libfuse.
    ///
    /// This creates a polling thread in the background as a reactor since tokio does not provide a
    /// way to access `POLLERR` explicitly or at all on the read side.
    pub(crate) fn from_raw(fd: RawFd) -> io::Result<Self> {
        let state = Arc::new(PollState::default());

        let poll = Arc::new(Epoll::new()?);

        let thread = std::thread::spawn({
            let state = Arc::clone(&state);
            let poll = Arc::clone(&poll);
            move || poll_thread_main(poll, fd, state)
        });

        let this = Self {
            fd,
            state,
            thread: Some(thread),
            poll,
        };

        // make sure it is nonblocking
        unsafe {
            let rc = libc::fcntl(fd, libc::F_GETFL);
            if rc == -1 {
                return Err(io::Error::last_os_error());
            }

            let rc = libc::fcntl(fd, libc::F_SETFL, rc | libc::O_NONBLOCK);
            if rc == -1 {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(this)
    }

    /// This moves the ready state into the ReadyGuard which will return it back into the
    /// ready-state unless `.clear_ready()` was used to mark the encounter of an -EAGAIN.
    ///
    /// We need to move it out since the polling thread runs in parallel and is edge triggered,
    /// meaning that if we get `READY_IN` and perform multiple `read()` calls, there's a race
    /// between seeing `-EAGAIN` and clearing the ready-bits where the polling thread adds them in.
    pub(crate) fn poll_read_ready(&self, cx: &mut Context) -> Poll<io::Result<ReadyGuard>> {
        let ready = self
            .state
            .ready
            .fetch_and(!(READY_IN | READY_ERR), Ordering::AcqRel);

        if ready & READY_FIN_ERR != 0 {
            return Poll::Ready(Err(self.state.error.lock().unwrap().take().unwrap()));
        }
        if ready != 0 {
            return Poll::Ready(Ok(ReadyGuard::new(self, ready)));
        }

        // lock
        let mut state_waker = self.state.waker.lock().unwrap();

        // but then check again to avoid a race where after our fetch the reactor sets the bits
        // *and* is first to locking the waker mutex
        let ready = self
            .state
            .ready
            .fetch_and(!(READY_IN | READY_ERR), Ordering::AcqRel);

        if ready & READY_FIN_ERR != 0 {
            return Poll::Ready(Err(self.state.error.lock().unwrap().take().unwrap()));
        }
        if ready != 0 {
            return Poll::Ready(Ok(ReadyGuard::new(self, ready)));
        }

        *state_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsRawFd for FuseFd {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

/// This holds the ready bits and, unless cleared, puts them back into the `FuseFd`'s ready state
/// on drop.
///
/// This is the equivalent of tokio's `AsyncFdReadyGuard`.
#[must_use]
pub(crate) struct ReadyGuard<'a> {
    ready: u32,
    fd: &'a FuseFd,
}

impl Drop for ReadyGuard<'_> {
    fn drop(&mut self) {
        // unless we encountered `-EAGAIN` we need to *keep* the ready state bits set, since the
        // polling thread only reacts to *edges* (EPOLLET).
        if self.ready != 0 {
            self.fd.state.ready.fetch_or(self.ready, Ordering::AcqRel);
        }
    }
}

impl<'a> ReadyGuard<'a> {
    fn new(fd: &'a FuseFd, ready: u32) -> Self {
        Self { ready, fd }
    }

    pub(crate) fn clear_ready(&mut self) {
        self.ready = 0;
    }

    /// Fuse signals unmounting via `EPOLLERR`.
    pub(crate) fn is_unmounted(&self) -> bool {
        self.ready & READY_ERR != 0
    }

    pub(crate) fn is_eof(&self) -> bool {
        self.ready & READY_FIN != 0
    }
}

/// The ready state accessed by both the reactor thread and the `FuseFd`.
#[derive(Default)]
struct PollState {
    /// Ready bits.
    ready: AtomicU32,

    /// If an error happens in the reactor, `READY_ERR` is raised and this option is set.
    /// It is also set when `READY_FIN_ERR` is set.
    error: Mutex<Option<io::Error>>,

    /// When the `FuseFd` gets polled via `poll_read_ready()`, the waker is stored here.
    ///
    /// The reactor thread uses it to wake up the read future on `POLLIN` and `POLLERR`.
    waker: Mutex<Option<Waker>>,
}

// helpers to avoid casting:
const EPOLLIN: u32 = libc::EPOLLIN as u32;
const EPOLLERR: u32 = libc::EPOLLERR as u32;
const EPOLLET: u32 = libc::EPOLLET as u32;

// beside `EPOLLIN` and `EPOLLERR` we also want to have bits to say that the reactor thread has
// finished or encountered an error, so we map the `EPOLLIN`/`EPOLLERR` values to our own:

/// `EPOLLIN` was set
const READY_IN: u32 = 0b1;
/// `EPOLLERR` was set
const READY_ERR: u32 = 0b10;
/// The reactor thread has ended.
const READY_FIN: u32 = 0b100;
/// The reactor thread encountered an error.
const READY_FIN_ERR: u32 = 0b1000;

/// Map `EPOLL*` to `READY_*`
fn epoll_to_ready(value: u32) -> u32 {
    let mut out = 0;
    if value & EPOLLIN != 0 {
        out |= READY_IN
    }
    if value & EPOLLERR != 0 {
        out |= READY_ERR
    }
    out
}

/// An `epoll` file descriptor with an `eventfd` used to "wake up" the reactor.
struct Epoll {
    /// the epoll fd
    epfd: RawFd,

    /// eventfd used to wake up the polling thread when we close the fuse fd
    finish: RawFd,
}

impl Drop for Epoll {
    fn drop(&mut self) {
        if self.epfd >= 0 {
            let _ = unsafe { libc::close(self.epfd) };
        }
        if self.finish >= 0 {
            let _ = unsafe { libc::close(self.finish) };
        }
    }
}

impl Epoll {
    /// Create a new instance.
    fn new() -> io::Result<Self> {
        let mut this = Epoll {
            epfd: -1,
            finish: -1,
        };

        this.epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if this.epfd < 0 {
            return Err(io::Error::last_os_error());
        }

        this.finish = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if this.finish < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(this)
    }

    /// Wake up and shutdown the reactor.
    fn signal_finish(&self) -> io::Result<()> {
        let buf = 1u64.to_ne_bytes();

        let rc = unsafe { libc::write(self.finish, &buf[0] as *const u8 as _, 8) };
        if rc != 8 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// `epoll_ctl` wrapper.
    fn ctl(&self, ctl: libc::c_int, fd: RawFd, events: u32, data: u64) -> io::Result<()> {
        let rc = unsafe {
            libc::epoll_ctl(
                self.epfd,
                ctl,
                fd,
                &mut libc::epoll_event { events, u64: data },
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `epoll_wait` wrapper.
    fn wait(&self, events: &mut [libc::epoll_event]) -> io::Result<usize> {
        let rc = unsafe { libc::epoll_wait(self.epfd, &mut events[0], events.len() as i32, -1) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc as usize)
        }
    }
}

/// Main entry point for the polling thread (reactor).
///
/// Calls out to `poll_thread_inner` and deals with its errors.
fn poll_thread_main(poll: Arc<Epoll>, fd: RawFd, state: Arc<PollState>) {
    match poll_thread_inner(poll, fd, Arc::clone(&state)) {
        Ok(()) => (),
        Err(err) => {
            *state.error.lock().unwrap() = Some(err);
            state.ready.fetch_or(READY_FIN_ERR, Ordering::AcqRel);
        }
    }

    state.ready.fetch_or(READY_FIN, Ordering::AcqRel);
    if let Some(waker) = state.waker.lock().unwrap().take() {
        waker.wake();
    }
}

/// Actual reactor loop.
fn poll_thread_inner(poll: Arc<Epoll>, fd: RawFd, state: Arc<PollState>) -> io::Result<()> {
    const TOKEN: u64 = 0;
    const FINISH_TOKEN: u64 = 1;

    // Setup the fuse fd and eventfd for wake-ups on input.
    poll.ctl(libc::EPOLL_CTL_ADD, fd, EPOLLIN | EPOLLERR | EPOLLET, TOKEN)?;
    poll.ctl(
        libc::EPOLL_CTL_ADD,
        poll.finish,
        EPOLLIN | EPOLLET,
        FINISH_TOKEN,
    )?;

    // arbitrary buffer size
    let mut events: [libc::epoll_event; 8] = unsafe { std::mem::zeroed() };

    'outer: loop {
        let rc = poll.wait(&mut events)?;
        if rc >= events.len() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "epoll_wait returned garbage",
            ));
        }

        for event in &events[0..rc] {
            match event.u64 {
                // eventfd got triggered, we just stop at this point
                FINISH_TOKEN => break 'outer,

                // fuse fd has something to read, set the ready bits and wake up any waiters
                TOKEN => {
                    state
                        .ready
                        .fetch_or(epoll_to_ready(event.events), Ordering::AcqRel);

                    if event.events & EPOLLERR != 0 {
                        // The fuse file system was unmounted, let's finish up!
                        break 'outer;
                    }

                    if let Some(waker) = state.waker.lock().unwrap().take() {
                        waker.wake();
                    }
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "epoll_wait returned unexpected events",
                    ))
                }
            }
        }
    }

    Ok(())
}
