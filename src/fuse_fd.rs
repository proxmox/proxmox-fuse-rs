//! This binds the fuse file descriptor to the tokio reactor.

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

use mio::event::Evented;
use mio::unix::EventedFd;
use mio::{Poll, PollOpt, Ready, Token};

pub struct FuseFd {
    fd: RawFd,
}

impl FuseFd {
    pub(crate) fn from_raw(fd: RawFd) -> io::Result<Self> {
        let this = Self { fd };

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
}

impl AsRawFd for FuseFd {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Evented for FuseFd {
    fn register(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.fd).register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.fd).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        EventedFd(&self.fd).deregister(poll)
    }
}
