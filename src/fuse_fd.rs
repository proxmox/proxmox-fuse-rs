//! This binds the fuse file descriptor to the tokio reactor.

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

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
