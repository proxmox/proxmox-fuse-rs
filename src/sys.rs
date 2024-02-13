//! libfuse3 bindings

use std::ffi::CStr;
use std::io;
use std::marker::PhantomData;

use libc::{c_char, c_int, c_void, off_t, size_t};

/// Node ID of the root i-node. This is fixed according to the FUSE API.
pub const ROOT_ID: u64 = 1;

/// FFI types for easier readability
pub type RawRequest = *mut c_void;
pub type MutPtr = *mut c_void;
pub type ConstPtr = *const c_void;
pub type StrPtr = *const c_char;
pub type MutStrPtr = *mut c_char;

/// To help us out with auto-trait implementations:
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Request {
    raw: RawRequest,
}

impl Request {
    pub const NULL: Self = Self {
        raw: std::ptr::null_mut(),
    };

    #[inline]
    pub fn is_null(&self) -> bool {
        self.raw.is_null()
    }
}

unsafe impl Send for Request {}
unsafe impl Sync for Request {}

/// Command line arguments passed to fuse.
#[repr(C)]
#[derive(Debug)]
pub struct FuseArgs<'a> {
    argc: c_int,
    argv: *const StrPtr,
    allocated: c_int,
    _phantom: PhantomData<&'a [*const StrPtr]>,
}

impl<'a> From<&'a [*const c_char]> for FuseArgs<'a> {
    fn from(slice: &[*const c_char]) -> Self {
        Self {
            argc: slice.len() as c_int,
            argv: slice.as_ptr(),
            allocated: 0,
            _phantom: PhantomData,
        }
    }
}

#[rustfmt::skip]
#[link(name = "fuse3")]
extern "C" {
    pub fn fuse_session_new(args: Option<&FuseArgs>, oprs: Option<&Operations>, size: size_t, op: ConstPtr) -> MutPtr;
    pub fn fuse_session_fd(session: ConstPtr) -> c_int;
    pub fn fuse_session_mount(session: ConstPtr, mountpoint: StrPtr) -> c_int;
    pub fn fuse_session_unmount(session: ConstPtr);
    pub fn fuse_session_destroy(session: ConstPtr);
    pub fn fuse_reply_attr(req: Request, attr: Option<&libc::stat>, timeout: f64) -> c_int;
    pub fn fuse_reply_err(req: Request, errno: c_int) -> c_int;
    pub fn fuse_reply_buf(req: Request, buf: *const c_char, size: size_t) -> c_int;
    pub fn fuse_reply_iov(req: Request, iov: *const std::io::IoSlice<'_>, count: c_int) -> c_int;
    pub fn fuse_reply_entry(req: Request, entry: Option<&EntryParam>) -> c_int;
    pub fn fuse_reply_create(req: Request, entry: Option<&EntryParam>, file_info: *const FuseFileInfo) -> c_int;
    pub fn fuse_reply_open(req: Request, file_info: *const FuseFileInfo) -> c_int;
    pub fn fuse_reply_xattr(req: Request, size: size_t) -> c_int;
    pub fn fuse_reply_readlink(req: Request, link: StrPtr) -> c_int;
    pub fn fuse_reply_none(req: Request);
    pub fn fuse_reply_write(req: Request, count: libc::size_t) -> c_int;
    pub fn fuse_req_userdata(req: Request) -> MutPtr;
    pub fn fuse_add_direntry_plus(req: Request, buf: MutStrPtr, bufsize: size_t, name: StrPtr, stbuf: Option<&EntryParam>, off: c_int) -> size_t;
    pub fn fuse_add_direntry(req: Request, buf: MutStrPtr, bufsize: size_t, name: StrPtr, stbuf: Option<&libc::stat>, off: c_int) -> size_t;
    pub fn fuse_session_process_buf(session: ConstPtr, buf: Option<&FuseBuf>);
    pub fn fuse_session_receive_buf(session: ConstPtr, buf: Option<&mut FuseBuf>) -> c_int;
}

// Generate a `const Operations::DEFAULT` we can use as `..DEFAULT` when not implementing every
// single call.
macro_rules! default_to_none {
    (
        $(#[$attr:meta])*
        pub struct $name:ident { $(pub $field:ident : $ty:ty,)* }
    ) => (
        $(#[$attr])*
        pub struct $name {
            $(pub $field : $ty,)*
        }

        impl $name {
            pub const DEFAULT: Self = Self {
                $($field : None,)*
            };
        }
    );
}

#[rustfmt::skip]
default_to_none! {
    /// `Operations` defines the callback function table of supported operations.
    #[repr(C)]
    #[derive(Default)]
    pub struct Operations {
        // The order in which the functions are listed matters, as the offset in the
        // struct defines what function the fuse driver uses.
        // It should therefore not be altered!
        pub init:            Option<extern fn(userdata: MutPtr)>,
        pub destroy:         Option<extern fn(userdata: MutPtr)>,
        pub lookup:          Option<extern fn(req: Request, parent: u64, name: StrPtr)>,
        pub forget:          Option<extern fn(req: Request, inode: u64, nlookup: u64)>,
        pub getattr:         Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo)>,
        pub setattr:         Option<extern fn(req: Request, inode: u64, attr: *const libc::stat, to_set: c_int, file_info: *const FuseFileInfo)>,
        pub readlink:        Option<extern fn(req: Request, inode: u64)>,
        pub mknod:           Option<extern fn(req: Request, parent: u64, name: StrPtr, mode: libc::mode_t, rdev: libc::dev_t)>,
        pub mkdir:           Option<extern fn(req: Request, parent: u64, name: StrPtr, mode: libc::mode_t)>,
        pub unlink:          Option<extern fn(req: Request, parent: u64, name: StrPtr)>,
        pub rmdir:           Option<extern fn(req: Request, parent: u64, name: StrPtr)>,
        pub symlink:         Option<extern fn(req: Request, link: StrPtr, parent: u64, name: StrPtr)>,
        pub rename:          Option<extern fn(req: Request, parent: u64, name: StrPtr, newparent: u64, newname: StrPtr, flags: c_int)>,
        pub link:            Option<extern fn(req: Request, inode: u64, newparent: u64, newname: StrPtr)>,
        pub open:            Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo)>,
        pub read:            Option<extern fn(req: Request, inode: u64, size: size_t, offset: libc::off_t, file_info: *const FuseFileInfo)>,
        pub write:           Option<extern fn(req: Request, inode: u64, buffer: *const u8, size: size_t, offset: libc::off_t, file_info: *const FuseFileInfo)>,
        pub flush:           Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo)>,
        pub release:         Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo)>,
        pub fsync:           Option<extern fn(req: Request, inode: u64, datasync: c_int, file_info: *const FuseFileInfo)>,
        pub opendir:         Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo)>,
        pub readdir:         Option<extern fn(req: Request, inode: u64, size: size_t, offset: off_t, file_info: *const FuseFileInfo)>,
        pub releasedir:      Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo)>,
        pub fsyncdir:        Option<extern fn(req: Request, inode: u64, datasync: c_int, file_info: *const FuseFileInfo)>,
        pub statfs:          Option<extern fn(req: Request, inode: u64)>,
        pub setxattr:        Option<extern fn(req: Request, inode: u64, name: StrPtr, value: StrPtr, size: size_t, flags: c_int)>,
        pub getxattr:        Option<extern fn(req: Request, inode: u64, name: StrPtr, size: size_t)>,
        pub listxattr:       Option<extern fn(req: Request, inode: u64, size: size_t)>,
        pub removexattr:     Option<extern fn(req: Request, inode: u64, name: StrPtr)>,
        pub access:          Option<extern fn(req: Request, inode: u64, mask: i32)>,
        pub create:          Option<extern fn(req: Request, parent: u64, name: StrPtr, mode: libc::mode_t, file_info: *const FuseFileInfo)>,
        pub getlk:           Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo, lock: MutPtr)>,
        pub setlk:           Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo, lock: MutPtr, sleep: c_int)>,
        pub bmap:            Option<extern fn(req: Request, inode: u64, blocksize: size_t, idx: u64)>,
        pub ioctl:           Option<extern fn(req: Request, inode: u64, cmd: c_int, arg: MutPtr, file_info: *const FuseFileInfo, flags: c_int, in_buf: ConstPtr, in_bufsz: size_t, out_bufsz: size_t)>,
        pub poll:            Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo, pollhandle: MutPtr)>,
        pub write_buf:       Option<extern fn(req: Request, inode: u64, bufv: MutPtr, offset: libc::off_t, file_info: *const FuseFileInfo)>,
        pub retrieve_reply:  Option<extern fn(req: Request, cookie: ConstPtr, inode: u64, offset: libc::off_t, bufv: MutPtr)>,
        pub forget_multi:    Option<extern fn(req: Request, count: size_t, forgets: MutPtr)>,
        pub flock:           Option<extern fn(req: Request, inode: u64, file_info: *const FuseFileInfo, op: c_int)>,
        pub fallocate:       Option<extern fn(req: Request, inode: u64, mode: c_int, offset: libc::off_t, length: libc::off_t, file_info: *const FuseFileInfo)>,
        pub readdirplus:     Option<extern fn(req: Request, inode: u64, size: size_t, offset: off_t, file_info: *const FuseFileInfo)>,
        pub copy_file_range: Option<extern fn(req: Request, ino_in: u64, off_in: libc::off_t, fi_in: *const FuseFileInfo, ino_out: u64, off_out: libc::off_t, fi_out: *const FuseFileInfo, len: size_t, flags: c_int)>,
    }
}

/// FUSE entry for fuse_reply_entry in lookup callback
#[repr(C)]
pub struct EntryParam {
    pub inode: u64,
    pub generation: u64,
    pub attr: libc::stat,
    pub attr_timeout: f64,
    pub entry_timeout: f64,
}

impl EntryParam {
    /// A simple entry has a maximum attribute/entry timeout value and always a generatio of 1.
    /// This is a convenience method used since we mostly use this for static unchangable archives.
    pub fn simple(inode: u64, attr: libc::stat) -> Self {
        Self {
            inode,
            generation: 1,
            attr,
            attr_timeout: f64::MAX,
            entry_timeout: f64::MAX,
        }
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct FuseBuf {
    /// Size of data in bytes
    size: size_t,

    /// Buffer flags
    flags: c_int,

    /// Memory pointer
    ///
    /// Used unless FUSE_BUF_IS_FD flag is set.
    mem: *mut c_void,

    /// File descriptor
    ///
    /// Used if FUSE_BUF_IS_FD flag is set.
    fd: c_int,

    /// File position
    ///
    /// Used if FUSE_BUF_FD_SEEK flag is set.
    pos: off_t,
}

unsafe impl Send for FuseBuf {}
unsafe impl Sync for FuseBuf {}

impl Drop for FuseBuf {
    fn drop(&mut self) {
        unsafe {
            libc::free(self.mem);
        }
    }
}

impl FuseBuf {
    pub fn new() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// This is used to communicate the result of an `Open` request.
/// This contains some C bitfields for which accessor methods are provided.
#[derive(Clone, Debug)]
#[repr(C)]
pub struct FuseFileInfo {
    /// Open flags. Available in open() and release()
    pub(crate) flags: c_int,

    /// Various bitfields for which we have C glue code in `glue.c`.
    _bits: u64,

    /// File handle.  May be filled in by filesystem in open().
    /// Available in all other file operations
    pub(crate) fh: u64,

    /// Lock owner id. Available in locking operations and flush.
    pub(crate) lock_owner: u64,

    /// Requested poll events. Available in ->poll. Only set on kernels
    /// which support it.  If unsupported, this field is set to zero.
    pub(crate) poll_events: u32,
}

macro_rules! fuse_file_info_accessors {
    ($(($flag:ident, $glue_set:ident, $glue_get:ident, $rust_set:ident, $rust_get:ident))+) => {
        #[link(name = "glue", kind = "static")]
        extern "C" {
            $(
            fn $glue_set(ffi: *mut FuseFileInfo, value: libc::c_uint);
            fn $glue_get(ffi: *mut FuseFileInfo) -> libc::c_uint;
            )+
        }

        impl FuseFileInfo {
            $(
            #[doc = concat!(
                "Set the `",
                stringify!($flag),
                "` flag. See fuse's `struct fuse_file_info` for details."
            )]
            pub fn $rust_set(&mut self, value: bool) {
                unsafe { $glue_set(self, value as _) }
            }

            #[doc = concat!(
                "Get the `",
                stringify!($flag),
                "` flag. See fuse's `struct fuse_file_info` for details."
            )]
            pub fn $rust_get(&mut self) -> bool {
                unsafe { $glue_get(self) != 0 }
            }
            )+
        }
    };
}

#[rustfmt::skip]
fuse_file_info_accessors! {
    (writepage,     glue_set_ffi_writepage,     glue_get_ffi_writepage,     set_writepage,     get_writepage)
    (direct_io,     glue_set_ffi_direct_io,     glue_get_ffi_direct_io,     set_direct_io,     get_direct_io)
    (flush,         glue_set_ffi_flush,         glue_get_ffi_flush,         set_flush,         get_flush)
    (nonseekable,   glue_set_ffi_nonseekable,   glue_get_ffi_nonseekable,   set_nonseekable,   get_nonseekable)
    (flock_release, glue_set_ffi_flock_release, glue_get_ffi_flock_release, set_flock_release, get_flock_release)
    (cache_readdir, glue_set_ffi_cache_readdir, glue_get_ffi_cache_readdir, set_cache_readdir, get_cache_readdir)
    (noflush,       glue_set_ffi_noflush,       glue_get_ffi_noflush,       set_noflush,       get_noflush)
}

#[rustfmt::skip]
pub mod setattr {
    pub const MODE      : libc::c_int = 1 << 0;
    pub const UID       : libc::c_int = 1 << 1;
    pub const GID       : libc::c_int = 1 << 2;
    pub const SIZE      : libc::c_int = 1 << 3;
    pub const ATIME     : libc::c_int = 1 << 4;
    pub const MTIME     : libc::c_int = 1 << 5;
    pub const ATIME_NOW : libc::c_int = 1 << 7;
    pub const MTIME_NOW : libc::c_int = 1 << 8;
    pub const CTIME     : libc::c_int = 1 << 10;
}

/// State of ReplyBuf after last add_entry call
#[must_use]
pub enum ReplyBufState {
    /// Entry was successfully added to ReplyBuf
    Ok,
    /// Entry did not fit into ReplyBuf, was not added
    Full,
}

impl ReplyBufState {
    #[inline]
    pub fn is_full(&self) -> bool {
        matches!(self, &ReplyBufState::Full)
    }
}

/// Used to correctly fill and reply the buffer for the readdirplus callback
pub struct ReplyBuf {
    /// internal buffer holding the binary data
    buffer: Vec<u8>,
    /// offset up to which the buffer is filled already
    filled: usize,
    /// fuse request the buffer is used to reply to
    request: Request,
}

impl std::fmt::Debug for ReplyBuf {
    fn fmt(&self, _f: &mut std::fmt::Formatter) -> std::fmt::Result {
        Ok(())
    }
}

impl ReplyBuf {
    /// Create a new empty `ReplyBuf` of `size` with element counting index at `next`.
    pub fn new(request: Request, size: usize) -> Self {
        let buffer = unsafe {
            let data = std::alloc::alloc(std::alloc::Layout::array::<u8>(size).unwrap());
            Vec::from_raw_parts(data, size, size)
        };
        Self {
            buffer,
            filled: 0,
            request,
        }
    }

    /// Send the reply with what we have buffered so far.
    pub fn reply(mut self) -> io::Result<()> {
        let rc = unsafe {
            let ptr = self.buffer.as_mut_ptr() as *mut c_char;
            fuse_reply_buf(self.request, ptr, self.filled)
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(-rc))
        }
    }

    fn after_add(&mut self, entry_size: usize) -> ReplyBufState {
        let filled = self.filled + entry_size;

        if filled > self.buffer.len() {
            ReplyBufState::Full
        } else {
            self.filled = filled;
            ReplyBufState::Ok
        }
    }

    pub fn add_readdir_plus(
        &mut self,
        name: &CStr,
        attr: &EntryParam,
        next: isize,
    ) -> ReplyBufState {
        let size = unsafe {
            let buffer = &mut self.buffer[self.filled..];
            fuse_add_direntry_plus(
                self.request,
                buffer.as_mut_ptr() as *mut c_char,
                buffer.len(),
                name.as_ptr(),
                Some(attr),
                next as c_int,
            ) as usize
        };
        self.after_add(size)
    }

    pub fn add_readdir(&mut self, name: &CStr, attr: &libc::stat, next: isize) -> ReplyBufState {
        let size = unsafe {
            let buffer = &mut self.buffer[self.filled..];
            fuse_add_direntry(
                self.request,
                buffer.as_mut_ptr() as *mut c_char,
                buffer.len(),
                name.as_ptr(),
                Some(attr),
                next as c_int,
            ) as usize
        };
        self.after_add(size)
    }
}
