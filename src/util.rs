//! Some helpers.

use std::fmt;

/// Helper for `Debug` derives.
#[derive(Clone)]
pub struct Stat {
    stat: libc::stat,
}

impl From<libc::stat> for Stat {
    fn from(stat: libc::stat) -> Self {
        Self { stat }
    }
}

impl std::ops::Deref for Stat {
    type Target = libc::stat;

    fn deref(&self) -> &Self::Target {
        &self.stat
    }
}

impl std::ops::DerefMut for Stat {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stat
    }
}

impl fmt::Debug for Stat {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        // don't care much about more fields than these:
        fmt.debug_struct("stat")
            .field("st_ino", &self.stat.st_ino)
            .field("st_mode", &self.stat.st_mode)
            .field("st_uid", &self.stat.st_uid)
            .field("st_gid", &self.stat.st_gid)
            .field("st_rdev", &self.stat.st_rdev)
            .field("st_size", &self.stat.st_size)
            .field("st_ctime", &self.stat.st_ctime)
            .field("st_mtime", &self.stat.st_mtime)
            .field("st_atime", &self.stat.st_atime)
            .finish()
    }
}
