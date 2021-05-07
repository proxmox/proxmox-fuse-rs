//! The bigger part of the public API is about which requests we're handling:
//!
//! Currently all reply functions are regular functions returning an `io::Result`, however, it is
//! possible that in the future these will become `async fns`, depending on whether the fuse file
//! descriptor can actually fill up when it is non-blocking?

use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::Duration;

use crate::sys::{self, ReplyBufState};
use crate::util::Stat;

#[derive(Debug)]
pub struct RequestGuard {
    raw: sys::Request,
}

unsafe impl Send for RequestGuard {}
unsafe impl Sync for RequestGuard {}

impl RequestGuard {
    pub(crate) fn from_raw(raw: sys::Request) -> Self {
        Self { raw }
    }

    /// Consume the request and to not trigger the automatic `ENOSYS` response.
    pub fn into_raw(mut self) -> sys::Request {
        std::mem::replace(&mut self.raw, sys::Request::NULL)
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                let _ = sys::fuse_reply_err(self.raw, libc::ENOSYS);
            }
        }
    }
}

fn reply_err(request: RequestGuard, errno: libc::c_int) -> io::Result<()> {
    unsafe {
        let rc = sys::fuse_reply_err(request.into_raw(), errno);
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(-rc))
        }
    }
}

macro_rules! reply_result {
    ($self:ident : $expr:expr) => {{
        let rc = unsafe { $expr };
        if rc == 0 {
            let _done = $self.request.into_raw();
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(-rc))
        }
    }};
}

/// Helper trait to easily provide the fail method for all the request types within the `Request`
/// enum even after they have been moved out of the enum, without requiring the exact type.
pub trait FuseRequest: Sized {
    /// Send an error reply.
    fn fail(self, errno: libc::c_int) -> io::Result<()>;

    /// Convenience method to use an `io::Error` as a response.
    fn io_fail(self, error: io::Error) -> io::Result<()> {
        self.fail(error.raw_os_error().unwrap_or(libc::EIO))
    }

    // /// Wrap code so that `std::io::Errors` get sent as a reply and other errors (including errors
    // /// sending the reply) will propagate through as errors.
    // fn wrap<F, E>(self, func: F) -> io::Result<()>
    // where
    //     F: FnOnce(Self) -> Result<(), E>,
    //     E: std::error::Error + 'static,
    // {
    //     match func(self) {
    //         Ok(()) => Ok(()),
    //         Err(err) => {
    //             if let Some(err) = err.downcast_ref::<io::Error>() => {
    //                 self.fail(err.raw_os_error().unwrap_or(libc::EIO))
    //             } else {
    //                 Err(err)
    //             }
    //         }
    //     }
    // }
}

#[derive(Debug)]
pub enum Request {
    Lookup(Lookup),
    Forget(Forget),
    Getattr(Getattr),
    Setattr(Setattr),
    Readdir(Readdir),
    ReaddirPlus(ReaddirPlus),
    Mkdir(Mkdir),
    Create(Create),
    Mknod(Mknod),
    Open(Open),
    Release(Release),
    Read(Read),
    Unlink(Unlink),
    Rmdir(Rmdir),
    Write(Write),
    Readlink(Readlink),
    ListXAttrSize(ListXAttrSize),
    ListXAttr(ListXAttr),
    GetXAttrSize(GetXAttrSize),
    GetXAttr(GetXAttr),
    // NOTE:
    // Open:
    //     will need to create `FuseFileInfo.fh`, for which we'll probably add a trait generic
    //     parameter to the `Fuse` object with a `set` method which we call in the `open`
    //     *callback* already (as the `struct fuse_file_info` becomes invalid after any callback
    //     returns), and methods to get/delete the data. This will be either a generic type on
    //     `Fuse` itself, or we provide an &dyn or Box<dyn> for this.
    // Flush: will need `FuseFileInfo.lock_owner`
    // Poll: will need `FuseFileInfo.poll_events`
}

impl FuseRequest for Request {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        match self {
            Request::Forget(r) => {
                r.reply();
                Ok(())
            }
            Request::Lookup(r) => r.fail(errno),
            Request::Getattr(r) => r.fail(errno),
            Request::Setattr(r) => r.fail(errno),
            Request::Readdir(r) => r.fail(errno),
            Request::ReaddirPlus(r) => r.fail(errno),
            Request::Mkdir(r) => r.fail(errno),
            Request::Create(r) => r.fail(errno),
            Request::Mknod(r) => r.fail(errno),
            Request::Open(r) => r.fail(errno),
            Request::Release(r) => r.fail(errno),
            Request::Read(r) => r.fail(errno),
            Request::Unlink(r) => r.fail(errno),
            Request::Rmdir(r) => r.fail(errno),
            Request::Write(r) => r.fail(errno),
            Request::Readlink(r) => r.fail(errno),
            Request::ListXAttrSize(r) => r.fail(errno),
            Request::ListXAttr(r) => r.fail(errno),
            Request::GetXAttrSize(r) => r.fail(errno),
            Request::GetXAttr(r) => r.fail(errno),
        }
    }
}

/// A lookup for an entry in a directory. This should increase the lookup count for the inode,
/// as from then on the kernel will refer to the looked-up entry only via the inode..
#[derive(Debug)]
pub struct Lookup {
    pub(crate) request: RequestGuard,
    pub parent: u64,
    pub file_name: OsString,
}

impl FuseRequest for Lookup {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Lookup {
    pub fn reply(self, entry: &sys::EntryParam) -> io::Result<()> {
        let rc = unsafe { sys::fuse_reply_entry(self.request.raw, Some(entry)) };
        if rc == 0 {
            let _done = self.request.into_raw();
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(-rc))
        }
    }
}

/// Forget references (lookup count) for an inode. Once an inode reaches a lookup count of zero,
/// the kernel will not refer to the inode anymore, meaning any cached information to access it may
/// be released.
#[derive(Debug)]
pub struct Forget {
    pub(crate) request: RequestGuard,
    pub inode: u64,
    pub count: u64,
}

impl FuseRequest for Forget {
    /// Forget cannot fail.
    fn fail(self, _errno: libc::c_int) -> io::Result<()> {
        Ok(())
    }
}

impl Forget {
    pub fn reply(self) {
        unsafe {
            sys::fuse_reply_none(self.request.into_raw());
        }
    }
}

/// This is the equivalent of a `stat` call.
///
/// The inode is already known, so no changes to the lookup count occur.
#[derive(Debug)]
pub struct Getattr {
    pub(crate) request: RequestGuard,
    pub inode: u64,
}

impl FuseRequest for Getattr {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Getattr {
    /// Send a reply for a `Getattr` request.
    pub fn reply(self, stat: &libc::stat, timeout: f64) -> io::Result<()> {
        reply_result!(self: sys::fuse_reply_attr(self.request.raw, Some(stat), timeout))
    }
}

/// Get the contents of a directory without changing any lookup counts. (Contrary to
/// `ReaddirPlus`).
#[derive(Debug)]
pub struct Readdir {
    pub(crate) request: Option<RequestGuard>,
    pub inode: u64,
    pub offset: i64,

    reply_buffer: Option<sys::ReplyBuf>,
}

impl Drop for Readdir {
    fn drop(&mut self) {
        if self.reply_buffer.is_some() {
            let _ = reply_err(self.request.take().unwrap(), libc::EIO);
        }
    }
}

impl FuseRequest for Readdir {
    fn fail(mut self, errno: libc::c_int) -> io::Result<()> {
        self.reply_buffer = None;
        reply_err(self.request.take().unwrap(), errno)
    }
}

impl Readdir {
    pub(crate) fn new(request: RequestGuard, inode: u64, size: usize, offset: i64) -> Self {
        let raw_request = request.raw;

        Self {
            request: Some(request),
            inode,
            offset,
            reply_buffer: Some(sys::ReplyBuf::new(raw_request, size)),
        }
    }

    /// Add a reply entry. Note that unless you also consume the `Readdir` object with a call to
    /// the `reply()` method, this will produce an `EIO` error.
    pub fn add_entry(
        &mut self,
        name: &OsStr,
        stat: &libc::stat,
        next: isize,
    ) -> io::Result<ReplyBufState> {
        let name = CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "tried to reply with invalid file name",
            )
        })?;

        Ok(self
            .reply_buffer
            .as_mut()
            .unwrap()
            .add_readdir(&name, stat, next))
    }

    /// Send a successful reply. This also works if no entries have been added via `add_entry`,
    /// indicating an empty directory without `.` or `..` present.
    pub fn reply(mut self) -> io::Result<()> {
        let _disable_guard = self.request.take().unwrap().into_raw();
        self.reply_buffer.take().unwrap().reply()
    }
}

/// Lookup all the contents of a directory. On success, the lookup count of all the returned
/// entries should be increased by 1.
#[derive(Debug)]
pub struct ReaddirPlus {
    pub(crate) request: Option<RequestGuard>,
    pub inode: u64,
    pub offset: i64,

    reply_buffer: Option<sys::ReplyBuf>,
}

impl Drop for ReaddirPlus {
    fn drop(&mut self) {
        if self.reply_buffer.is_some() {
            let _ = reply_err(self.request.take().unwrap(), libc::EIO);
        }
    }
}

impl FuseRequest for ReaddirPlus {
    fn fail(mut self, errno: libc::c_int) -> io::Result<()> {
        self.reply_buffer = None;
        reply_err(self.request.take().unwrap(), errno)
    }
}

impl ReaddirPlus {
    pub(crate) fn new(request: RequestGuard, inode: u64, size: usize, offset: i64) -> Self {
        let raw_request = request.raw;

        Self {
            request: Some(request),
            inode,
            offset,
            reply_buffer: Some(sys::ReplyBuf::new(raw_request, size)),
        }
    }

    /// Add a reply entry. Note that unless you also consume the `ReaddirPlus` object with a call
    /// to the `reply()` method, this will produce an `EIO` error.
    pub fn add_entry(
        &mut self,
        name: &OsStr,
        stat: &libc::stat,
        next: isize,
        generation: u64,
        attr_timeout: f64,
        entry_timeout: f64,
    ) -> io::Result<ReplyBufState> {
        let name = CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "tried to reply with invalid file name",
            )
        })?;

        let entry = sys::EntryParam {
            inode: stat.st_ino,
            generation,
            attr: *stat,
            attr_timeout,
            entry_timeout,
        };

        Ok(self
            .reply_buffer
            .as_mut()
            .unwrap()
            .add_readdir_plus(&name, &entry, next))
    }

    /// Send a successful reply. This also works if no entries have been added via `add_entry`,
    /// indicating an empty directory without `.` or `..` present.
    pub fn reply(mut self) -> io::Result<()> {
        let _disable_guard = self.request.take().unwrap().into_raw();
        self.reply_buffer.take().unwrap().reply()
    }
}

/// Create a new directory with a lookup count of 1.
#[derive(Debug)]
pub struct Mkdir {
    pub(crate) request: RequestGuard,
    pub parent: u64,
    pub dir_name: OsString,
    pub mode: libc::mode_t,
}

impl FuseRequest for Mkdir {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Mkdir {
    pub fn reply(self, entry: &sys::EntryParam) -> io::Result<()> {
        reply_result!(self: sys::fuse_reply_entry(self.request.raw, Some(entry)))
    }
}

/// Create a new file with a lookup count of 1.
#[derive(Debug)]
pub struct Create {
    pub(crate) request: RequestGuard,
    pub parent: u64,
    pub file_name: OsString,
    pub mode: libc::mode_t,
    pub(crate) file_info: sys::FuseFileInfo,
}

impl FuseRequest for Create {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Create {
    /// The `fh` provided here will be available in later requests for this file handle.
    pub fn reply(mut self, entry: &sys::EntryParam, fh: u64) -> io::Result<()> {
        self.file_info.fh = fh;
        reply_result!(self: sys::fuse_reply_create(self.request.raw, Some(entry), &self.file_info))
    }
}

/// Create a new node (file or device) with a lookup count of 1.
#[derive(Debug)]
pub struct Mknod {
    pub(crate) request: RequestGuard,
    pub parent: u64,
    pub file_name: OsString,
    pub mode: libc::mode_t,
    pub dev: libc::dev_t,
}

impl FuseRequest for Mknod {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Mknod {
    /// The lookup count should be bumped by this reply.
    pub fn reply(self, entry: &sys::EntryParam) -> io::Result<()> {
        reply_result!(self: sys::fuse_reply_entry(self.request.raw, Some(entry)))
    }
}

/// Open a file. This counts as one reference to the file and can be tracked separately or as part
/// of the lookup count. If dealing with opened files requires a kind of state, the `fh` parameter
/// on the `reply` method should point to that state, as it will be included in all requests
/// related to this handle.
#[derive(Debug)]
pub struct Open {
    pub(crate) request: RequestGuard,
    pub inode: u64,
    pub flags: libc::c_int,
    pub(crate) file_info: sys::FuseFileInfo,
}

impl FuseRequest for Open {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Open {
    /// The `fh` provided here will be available in later requests for this file handle.
    pub fn reply(mut self, entry: &sys::EntryParam, fh: u64) -> io::Result<()> {
        self.file_info.fh = fh;
        reply_result!(self: sys::fuse_reply_open(self.request.raw, Some(entry), &self.file_info))
    }
}

/// Release a reference to a file.
#[derive(Debug)]
pub struct Release {
    pub(crate) request: RequestGuard,
    pub inode: u64,
    pub fh: u64,
    pub flags: libc::c_int,
}

impl FuseRequest for Release {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Release {
    pub fn reply(self) -> io::Result<()> {
        reply_err(self.request, 0)
    }
}

/// Read from a file.
#[derive(Debug)]
pub struct Read {
    pub(crate) request: RequestGuard,
    pub inode: u64,
    pub fh: u64,
    pub size: usize,
    pub offset: u64,
}

impl FuseRequest for Read {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Read {
    pub fn reply(self, data: &[u8]) -> io::Result<()> {
        let ptr = data.as_ptr() as *const libc::c_char;
        reply_result!(self: sys::fuse_reply_buf(self.request.raw, ptr, data.len()))
    }
}

pub enum SetTime {
    /// The time should be set to the current time.
    Now,

    /// Time since the epoch.
    Time(Duration),
}

fn c_duration(secs: libc::time_t, nsecs: i64) -> Duration {
    Duration::new(secs as u64, nsecs as u32)
}

impl SetTime {
    /// Truncates nsecs!
    fn from_c(secs: libc::time_t, nsecs: i64) -> Self {
        SetTime::Time(c_duration(secs, nsecs))
    }
}

/// Set attributes of a file.
#[derive(Debug)]
pub struct Setattr {
    pub(crate) request: RequestGuard,
    pub inode: u64,
    pub fh: Option<u64>,
    pub to_set: libc::c_int,
    pub(crate) stat: Stat,
}

impl FuseRequest for Setattr {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Setattr {
    /// `Some` if the mode field should be modified.
    pub fn mode(&self) -> Option<libc::mode_t> {
        if (self.to_set & sys::setattr::MODE) != 0 {
            Some(self.stat.st_mode)
        } else {
            None
        }
    }

    /// `Some` if the uid field should be modified.
    pub fn uid(&self) -> Option<libc::uid_t> {
        if (self.to_set & sys::setattr::UID) != 0 {
            Some(self.stat.st_uid)
        } else {
            None
        }
    }

    /// `Some` if the gid field should be modified.
    pub fn gid(&self) -> Option<libc::gid_t> {
        if (self.to_set & sys::setattr::GID) != 0 {
            Some(self.stat.st_gid)
        } else {
            None
        }
    }

    /// `Some` if the size field should be modified.
    pub fn size(&self) -> Option<u64> {
        if (self.to_set & sys::setattr::SIZE) != 0 {
            Some(self.stat.st_size as u64)
        } else {
            None
        }
    }

    /// `Some` if the atime field should be modified.
    pub fn atime(&self) -> Option<SetTime> {
        if (self.to_set & sys::setattr::ATIME) != 0 {
            Some(SetTime::from_c(self.stat.st_atime, self.stat.st_atime_nsec))
        } else if (self.to_set & sys::setattr::ATIME_NOW) != 0 {
            Some(SetTime::Now)
        } else {
            None
        }
    }

    /// `Some` if the mtime field should be modified.
    pub fn mtime(&self) -> Option<SetTime> {
        if (self.to_set & sys::setattr::MTIME) != 0 {
            Some(SetTime::from_c(self.stat.st_mtime, self.stat.st_mtime_nsec))
        } else if (self.to_set & sys::setattr::MTIME_NOW) != 0 {
            Some(SetTime::Now)
        } else {
            None
        }
    }

    /// `Some` if the ctime field should be modified.
    pub fn ctime(&self) -> Option<Duration> {
        if (self.to_set & sys::setattr::CTIME) != 0 {
            Some(c_duration(self.stat.st_ctime, self.stat.st_ctime_nsec))
        } else {
            None
        }
    }

    /// Send a reply for a `Setattr` request.
    pub fn reply(self, stat: &libc::stat, timeout: f64) -> io::Result<()> {
        reply_result!(self: sys::fuse_reply_attr(self.request.raw, Some(stat), timeout))
    }
}

/// Remove a hard-link to a file. Note that the removed file may still have active references which
/// should still be usable. This only unlinks the file from one directory.
#[derive(Debug)]
pub struct Unlink {
    pub(crate) request: RequestGuard,
    pub parent: u64,
    pub file_name: OsString,
}

impl FuseRequest for Unlink {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Unlink {
    /// The `fh` provided here will be available in later requests for this file handle.
    pub fn reply(self) -> io::Result<()> {
        reply_err(self.request, 0)
    }
}

/// Remove a directory entry. Note that the removed directory may still have active references
/// which should still be usable. Only its hard link into the directory hierarchy is dropped.
#[derive(Debug)]
pub struct Rmdir {
    pub(crate) request: RequestGuard,
    pub parent: u64,
    pub dir_name: OsString,
}

impl FuseRequest for Rmdir {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Rmdir {
    /// The `fh` provided here will be available in later requests for this file handle.
    pub fn reply(self) -> io::Result<()> {
        reply_err(self.request, 0)
    }
}

/// Write to a file.
#[derive(Debug)]
pub struct Write {
    pub(crate) request: RequestGuard,
    pub inode: u64,
    pub fh: u64,
    pub data: *const u8,
    pub size: usize,
    pub offset: u64,

    /// We keep a reference count on the buffer we pass to `fuse_session_receive_buf` so it will
    /// not be cleared until it is used up by requests borrowing data from it, like `Write`.
    pub(crate) buffer: Arc<sys::FuseBuf>,
}

unsafe impl Send for Write {}
unsafe impl Sync for Write {}

impl FuseRequest for Write {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Write {
    pub fn reply(self, size: usize) -> io::Result<()> {
        reply_result!(self: sys::fuse_reply_write(self.request.raw, size))
    }

    #[inline]
    pub fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data, self.size) }
    }
}

/// Read a symbolic link.
#[derive(Debug)]
pub struct Readlink {
    pub(crate) request: RequestGuard,
    pub inode: u64,
}

impl FuseRequest for Readlink {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl Readlink {
    pub fn reply(self, data: &OsStr) -> io::Result<()> {
        let data = CString::new(data.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "tried to reply with invalid link")
        })?;

        self.c_reply(&data)
    }

    pub fn c_reply(self, data: &CStr) -> io::Result<()> {
        reply_result!(self: sys::fuse_reply_readlink(self.request.raw, data.as_ptr()))
    }
}

/// Get the size of the extended attribute list.
#[derive(Debug)]
pub struct ListXAttrSize {
    pub(crate) request: RequestGuard,
    pub inode: u64,
}

impl FuseRequest for ListXAttrSize {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl ListXAttrSize {
    pub fn reply(self, size: usize) -> io::Result<()> {
        reply_result!(self: sys::fuse_reply_xattr(self.request.raw, size))
    }
}

/// List extended attributes.
#[derive(Debug)]
pub struct ListXAttr {
    pub(crate) request: RequestGuard,
    pub inode: u64,
    pub size: usize,
    response: Vec<u8>,
}

impl FuseRequest for ListXAttr {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl ListXAttr {
    pub(crate) fn new(request: RequestGuard, inode: u64, size: usize) -> Self {
        Self {
            request,
            inode,
            size,
            response: Vec::new(),
        }
    }

    /// Check whether we can add `len` bytes to the buffer. This must already include the
    /// terminating zero.
    fn check_add(&self, len: usize) -> bool {
        self.response.len() + len <= len
    }

    /// Add an extended attribute entry to the response list.
    ///
    /// This returns `Full` if the entry would overflow the caller's buffer, but will not fail the
    /// request. Use `fail_full()` to send the default reply for a too-small buffer, or `reply()`
    /// to reply with success.
    pub fn add(&mut self, name: &OsStr) -> ReplyBufState {
        unsafe { self.add_bytes_without_zero(name.as_bytes()) }
    }

    /// Add a raw attribute name the response list. It must not contain any zeroes.
    ///
    /// See `add` for details.
    pub unsafe fn add_bytes_without_zero(&mut self, name: &[u8]) -> ReplyBufState {
        if !self.check_add(name.len() + 1) {
            return ReplyBufState::Full;
        }

        self.response.reserve(name.len() + 1);
        self.response.extend(name);
        self.response.push(0);

        ReplyBufState::Ok
    }

    /// Add a raw attribute name which is already zero terminated to the response list.
    ///
    /// See `add` for details.
    pub unsafe fn add_bytes_with_zero(&mut self, name: &[u8]) -> ReplyBufState {
        if !self.check_add(name.len() + 1) {
            return ReplyBufState::Full;
        }

        self.response.extend(name);

        ReplyBufState::Ok
    }

    /// Add a raw attribute name which is already zero terminated to the response list.
    ///
    /// This is the safe version as it uses a `CStr`.
    ///
    /// See `add` for details.
    pub fn add_c_string(&mut self, name: &CStr) -> ReplyBufState {
        unsafe { self.add_bytes_with_zero(name.to_bytes_with_nul()) }
    }

    /// Try to replace the current reply buffer with a raw data buffer. If the provided data is too
    /// large it will be returned as an error.
    pub fn set_raw_reply(&mut self, data: Vec<u8>) -> Result<(), Vec<u8>> {
        if data.len() > self.size {
            return Err(data);
        }

        self.response = data;
        Ok(())
    }

    /// Reply with the standard error for a too-small size.
    pub fn fail_full(self) -> io::Result<()> {
        self.fail(libc::ERANGE)
    }

    /// Reply to the request with the current data buffer.
    pub fn reply(self) -> io::Result<()> {
        let ptr = self.response.as_ptr() as *const libc::c_char;
        reply_result!(self: sys::fuse_reply_buf(self.request.raw, ptr, self.response.len()))
    }

    /// Reply to the request with either an error (`ERANGE` if the data doesn't fit) or with the
    /// provided raw buffer discarding anything previously added to the reply.
    pub fn reply_raw(self, data: &[u8]) -> io::Result<()> {
        if data.len() > self.size {
            return self.fail_full();
        }

        let ptr = data.as_ptr() as *const libc::c_char;
        reply_result!(self: sys::fuse_reply_buf(self.request.raw, ptr, data.len()))
    }
}

/// Get the size of an extended attribute.
#[derive(Debug)]
pub struct GetXAttrSize {
    pub(crate) request: RequestGuard,
    pub inode: u64,
    pub attr_name: OsString,
}

impl FuseRequest for GetXAttrSize {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl GetXAttrSize {
    pub fn reply(self, size: usize) -> io::Result<()> {
        reply_result!(self: sys::fuse_reply_xattr(self.request.raw, size))
    }
}

/// Get an extended attribute.
#[derive(Debug)]
pub struct GetXAttr {
    pub(crate) request: RequestGuard,
    pub inode: u64,
    pub attr_name: OsString,
    pub size: usize,
}

impl FuseRequest for GetXAttr {
    fn fail(self, errno: libc::c_int) -> io::Result<()> {
        reply_err(self.request, errno)
    }
}

impl GetXAttr {
    /// Reply to the request either with an error (`ERANGE` if the buffer doesn't fit), or with the
    /// provided data.
    pub fn reply(self, data: &[u8]) -> io::Result<()> {
        if data.len() > self.size {
            return self.fail(libc::ERANGE);
        }

        let ptr = data.as_ptr() as *const libc::c_char;
        reply_result!(self: sys::fuse_reply_buf(self.request.raw, ptr, data.len()))
    }
}
