use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{io, mem};

use anyhow::{bail, format_err, Error};
use futures::ready;
use tokio::io::PollEvented;
use tokio::stream::Stream;

use crate::fuse_fd::FuseFd;
use crate::requests::{self, Request, RequestGuard};
use crate::sys;
use crate::util::Stat;

/// The default set of operations enabled when nothing else is set via the `FuseSessionBuilder`
/// methods.
///
/// By default the stream can only yield `Gettattr` requests.
pub const DEFAULT_OPERATIONS: sys::Operations = sys::Operations {
    lookup: Some(FuseData::lookup),
    forget: Some(FuseData::forget),
    getattr: Some(FuseData::getattr),
    ..sys::Operations::DEFAULT
};

struct FuseData {
    /// We're assuming that it's possible `fuse_session_process_buf` may trigger multiple
    /// callbacks, so we need to enqueue them all,
    ///
    /// This is a `RefCell` since we're implementing `Stream` and therefore can only be polled by a
    /// single thread at a time. The requests get pushed here, and then immediately yielded by the
    /// `Stream::poll_next()` method.
    pending_requests: RefCell<VecDeque<Request>>,
    fbuf: Arc<sys::FuseBuf>,
}

unsafe impl Send for FuseData {}
unsafe impl Sync for FuseData {}

impl FuseData {
    extern "C" fn lookup(request: sys::Request, parent: u64, file_name: sys::StrPtr) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        let file_name = unsafe { CStr::from_ptr(file_name) };
        let file_name = OsStr::from_bytes(file_name.to_bytes()).to_owned();
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Lookup(requests::Lookup {
                request: RequestGuard::from_raw(request),
                parent,
                file_name,
            }));
    }

    extern "C" fn forget(request: sys::Request, inode: u64, nlookup: u64) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Forget(requests::Forget {
                request: RequestGuard::from_raw(request),
                inode,
                count: nlookup,
            }));
    }

    extern "C" fn getattr(request: sys::Request, inode: u64, _file_info: *const sys::FuseFileInfo) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Getattr(requests::Getattr {
                request: RequestGuard::from_raw(request),
                inode,
            }));
    }

    extern "C" fn readdir(
        request: sys::Request,
        inode: u64,
        size: libc::size_t,
        offset: libc::off_t,
        _file_info: *const sys::FuseFileInfo,
    ) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Readdir(requests::Readdir::new(
                RequestGuard::from_raw(request),
                inode,
                size,
                offset,
            )));
    }

    extern "C" fn readdirplus(
        request: sys::Request,
        inode: u64,
        size: libc::size_t,
        offset: libc::off_t,
        _file_info: *const sys::FuseFileInfo,
    ) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::ReaddirPlus(requests::ReaddirPlus::new(
                RequestGuard::from_raw(request),
                inode,
                size,
                offset,
            )));
    }

    extern "C" fn mkdir(
        request: sys::Request,
        parent: u64,
        dir_name: sys::StrPtr,
        mode: libc::mode_t,
    ) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        let dir_name = unsafe { CStr::from_ptr(dir_name) };
        let dir_name = OsStr::from_bytes(dir_name.to_bytes()).to_owned();
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Mkdir(requests::Mkdir {
                request: RequestGuard::from_raw(request),
                parent,
                dir_name,
                mode,
            }));
    }

    extern "C" fn create(
        request: sys::Request,
        parent: u64,
        file_name: sys::StrPtr,
        mode: libc::mode_t,
        file_info: *const sys::FuseFileInfo,
    ) {
        let (fuse_data, file_info, file_name) = unsafe {
            (
                &*(sys::fuse_req_userdata(request) as *mut FuseData),
                &*file_info,
                CStr::from_ptr(file_name),
            )
        };
        let file_name = OsStr::from_bytes(file_name.to_bytes()).to_owned();
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Create(requests::Create {
                request: RequestGuard::from_raw(request),
                parent,
                file_name,
                mode,
                file_info: file_info.clone(),
            }));
    }

    extern "C" fn mknod(
        request: sys::Request,
        parent: u64,
        file_name: sys::StrPtr,
        mode: libc::mode_t,
        dev: libc::dev_t,
    ) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        let file_name = unsafe { CStr::from_ptr(file_name) };
        let file_name = OsStr::from_bytes(file_name.to_bytes()).to_owned();
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Mknod(requests::Mknod {
                request: RequestGuard::from_raw(request),
                parent,
                file_name,
                mode,
                dev,
            }));
    }

    extern "C" fn open(request: sys::Request, inode: u64, file_info: *const sys::FuseFileInfo) {
        let (fuse_data, file_info) = unsafe {
            (
                &*(sys::fuse_req_userdata(request) as *mut FuseData),
                &*file_info,
            )
        };
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Open(requests::Open {
                request: RequestGuard::from_raw(request),
                inode,
                flags: file_info.flags,
                file_info: file_info.clone(),
            }));
    }

    extern "C" fn release(request: sys::Request, inode: u64, file_info: *const sys::FuseFileInfo) {
        let (fuse_data, file_info) = unsafe {
            (
                &*(sys::fuse_req_userdata(request) as *mut FuseData),
                &*file_info,
            )
        };
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Release(requests::Release {
                request: RequestGuard::from_raw(request),
                inode,
                flags: file_info.flags,
                fh: file_info.fh,
            }));
    }

    extern "C" fn read(
        request: sys::Request,
        inode: u64,
        size: libc::size_t,
        offset: libc::off_t,
        file_info: *const sys::FuseFileInfo,
    ) {
        let (fuse_data, file_info) = unsafe {
            (
                &*(sys::fuse_req_userdata(request) as *mut FuseData),
                &*file_info,
            )
        };
        let size = usize::from(size);
        let offset = offset as u64;
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Read(requests::Read {
                request: RequestGuard::from_raw(request),
                fh: file_info.fh,
                inode,
                size,
                offset,
            }));
    }

    extern "C" fn readlink(request: sys::Request, inode: u64) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Readlink(requests::Readlink {
                request: RequestGuard::from_raw(request),
                inode,
            }));
    }

    extern "C" fn setattr(
        request: sys::Request,
        inode: u64,
        stat: *const libc::stat,
        to_set: libc::c_int,
        file_info: *const sys::FuseFileInfo,
    ) {
        let (fuse_data, stat, file_info) = unsafe {
            (
                &*(sys::fuse_req_userdata(request) as *mut FuseData),
                &*stat,
                if file_info.is_null() {
                    None
                } else {
                    Some(&*file_info)
                },
            )
        };
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Setattr(requests::Setattr {
                request: RequestGuard::from_raw(request),
                inode,
                to_set,
                stat: Stat::from(stat.clone()),
                fh: file_info.map(|fi| fi.fh),
            }));
    }

    extern "C" fn unlink(request: sys::Request, parent: u64, file_name: sys::StrPtr) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        let file_name = unsafe { CStr::from_ptr(file_name) };
        let file_name = OsStr::from_bytes(file_name.to_bytes()).to_owned();
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Unlink(requests::Unlink {
                request: RequestGuard::from_raw(request),
                parent,
                file_name,
            }));
    }

    extern "C" fn rmdir(request: sys::Request, parent: u64, dir_name: sys::StrPtr) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        let dir_name = unsafe { CStr::from_ptr(dir_name) };
        let dir_name = OsStr::from_bytes(dir_name.to_bytes()).to_owned();
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Rmdir(requests::Rmdir {
                request: RequestGuard::from_raw(request),
                parent,
                dir_name,
            }));
    }

    extern "C" fn write(
        request: sys::Request,
        inode: u64,
        buffer: *const u8,
        size: libc::size_t,
        offset: libc::off_t,
        file_info: *const sys::FuseFileInfo,
    ) {
        let (fuse_data, file_info) = unsafe {
            (
                &*(sys::fuse_req_userdata(request) as *mut FuseData),
                &*file_info,
            )
        };
        let size = usize::from(size);
        let offset = offset as u64;
        fuse_data
            .pending_requests
            .borrow_mut()
            .push_back(Request::Write(requests::Write {
                request: RequestGuard::from_raw(request),
                fh: file_info.fh,
                inode,
                data: buffer,
                size,
                offset,
                buffer: Arc::clone(&fuse_data.fbuf),
            }));
    }

    extern "C" fn listxattr(request: sys::Request, inode: u64, size: libc::size_t) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        let size = usize::from(size);
        fuse_data.pending_requests.borrow_mut().push_back({
            if size == 0 {
                Request::ListXAttrSize(requests::ListXAttrSize {
                    request: RequestGuard::from_raw(request),
                    inode,
                })
            } else {
                Request::ListXAttr(requests::ListXAttr::new(
                    RequestGuard::from_raw(request),
                    inode,
                    size,
                ))
            }
        });
    }

    extern "C" fn getxattr(
        request: sys::Request,
        inode: u64,
        attr_name: sys::StrPtr,
        size: libc::size_t,
    ) {
        let fuse_data = unsafe { &*(sys::fuse_req_userdata(request) as *mut FuseData) };
        let attr_name = unsafe { CStr::from_ptr(attr_name) };
        let attr_name = OsStr::from_bytes(attr_name.to_bytes()).to_owned();
        let size = usize::from(size);
        fuse_data.pending_requests.borrow_mut().push_back({
            if size == 0 {
                Request::GetXAttrSize(requests::GetXAttrSize {
                    request: RequestGuard::from_raw(request),
                    inode,
                    attr_name,
                })
            } else {
                Request::GetXAttr(requests::GetXAttr {
                    request: RequestGuard::from_raw(request),
                    inode,
                    attr_name,
                    size,
                })
            }
        });
    }
}

pub struct FuseSessionBuilder {
    args: Vec<CString>,
    has_debug: bool,
    operations: sys::Operations,
}

impl FuseSessionBuilder {
    pub fn options(self, option: &str) -> Result<Self, Error> {
        Ok(self.options_c(
            CString::new(option).map_err(|err| format_err!("bad option string: {}", err))?,
        ))
    }

    pub fn options_os(self, option: &OsStr) -> Result<Self, Error> {
        Ok(self.options_c(
            CString::new(option.as_bytes())
                .map_err(|err| format_err!("bad option string: {}", err))?,
        ))
    }

    pub fn options_c(mut self, option: CString) -> Self {
        self.args.reserve(2);
        self.args.push(CString::new("-o").unwrap());
        self.args.push(option);
        self
    }

    pub fn debug(mut self) -> Self {
        if !self.has_debug {
            self.args.push(CString::new("--debug").unwrap());
        }
        self
    }

    pub fn build(self) -> Result<FuseSession, Error> {
        let args: Vec<*const libc::c_char> = self.args.iter().map(|cstr| cstr.as_ptr()).collect();

        let fuse_data = Box::new(FuseData {
            pending_requests: RefCell::new(VecDeque::new()),
            fbuf: Arc::new(sys::FuseBuf::new()),
        });
        let session = unsafe {
            sys::fuse_session_new(
                Some(&sys::FuseArgs::from(&args[..])),
                Some(&self.operations),
                mem::size_of_val(&self.operations),
                fuse_data.as_ref() as *const FuseData as sys::ConstPtr,
            )
        };
        drop(args);

        if session.is_null() {
            bail!("failed to create fuse session");
        }

        Ok(FuseSession {
            session,
            fuse_data: Some(fuse_data),
            mounted: false,
        })
    }

    /// Enable `Readdir` requests.
    pub fn enable_readdir(mut self) -> Self {
        self.operations.readdir = Some(FuseData::readdir);
        self
    }

    /// Enables all of `ReaddirPlus`, `Lookup` and `Forget` requests.
    ///
    /// The `Lookup` and `Forget` requests are required for reference counting implied by
    /// `ReaddirPlus`. The kernel should send `Forget` requests for references created via
    /// `ReaddirPlus`. Not handling them wouldn't make much sense.
    pub fn enable_readdirplus(mut self) -> Self {
        self.operations.readdirplus = Some(FuseData::readdirplus);
        self
    }

    /// Enable `Mkdir` requests.
    ///
    /// Note that the lookup count of newly created directory should be 1.
    pub fn enable_mkdir(mut self) -> Self {
        self.operations.mkdir = Some(FuseData::mkdir);
        self
    }

    /// Enable `Create`, `Open` and `Release` requests.
    ///
    /// Create and open a file.
    pub fn enable_create(mut self) -> Self {
        self.operations.create = Some(FuseData::create);
        self.enable_open()
    }

    /// Enable `Mknod`.
    ///
    /// This may be used by the kernel instead of `Create`.
    pub fn enable_mknod(mut self) -> Self {
        self.operations.mknod = Some(FuseData::mknod);
        self
    }

    /// Enable `Open` requests.
    ///
    /// Open a file.
    pub fn enable_open(mut self) -> Self {
        self.operations.open = Some(FuseData::open);
        self.operations.release = Some(FuseData::release);
        self
    }

    /// Enable `Setattr` requests.
    pub fn enable_setattr(mut self) -> Self {
        self.operations.setattr = Some(FuseData::setattr);
        self
    }

    /// Enable `Unlink` requests.
    pub fn enable_unlink(mut self) -> Self {
        self.operations.unlink = Some(FuseData::unlink);
        self
    }

    /// Enable `Rmdir` requests.
    pub fn enable_rmdir(mut self) -> Self {
        self.operations.rmdir = Some(FuseData::rmdir);
        self
    }

    /// Enable `Read` requests.
    pub fn enable_read(mut self) -> Self {
        self.operations.read = Some(FuseData::read);
        self
    }

    /// Enable `Write` requests.
    pub fn enable_write(mut self) -> Self {
        self.operations.write = Some(FuseData::write);
        self
    }

    /// Enable `Readlink` requests.
    pub fn enable_readlink(mut self) -> Self {
        self.operations.readlink = Some(FuseData::readlink);
        self
    }

    /// Enable requests to list extended attributes:
    ///
    /// * `ListXAttrSize`
    /// * `ListXAttr`
    /// * `GetXAttrSize`
    /// * `GetXAttr`
    pub fn enable_read_xattr(mut self) -> Self {
        self.operations.listxattr = Some(FuseData::listxattr);
        self.operations.getxattr = Some(FuseData::getxattr);
        self
    }
}

pub struct FuseSession {
    session: sys::MutPtr,
    fuse_data: Option<Box<FuseData>>,
    mounted: bool,
}

impl Drop for FuseSession {
    fn drop(&mut self) {
        unsafe {
            if self.mounted {
                let _ = sys::fuse_session_unmount(self.session);
            }

            if !self.session.is_null() {
                let _ = sys::fuse_session_destroy(self.session);
            }
        }
    }
}

impl FuseSession {
    pub fn mount(mut self, mountpoint: &Path) -> Result<Fuse, Error> {
        let mountpoint = mountpoint.canonicalize()?;
        let mountpoint = CString::new(mountpoint.as_os_str().as_bytes())
            .map_err(|err| format_err!("bad path for mount point: {}", err))?;

        let rc = unsafe { sys::fuse_session_mount(self.session, mountpoint.as_ptr()) };
        if rc != 0 {
            bail!("mount failed");
        }
        self.mounted = true;

        let fd = unsafe { sys::fuse_session_fd(self.session) };
        if fd < 0 {
            bail!("failed to get fuse session file descriptor");
        }

        let fuse_fd = PollEvented::new(FuseFd::from_raw(fd)?)?;

        // disable mount guard
        self.mounted = false;
        Ok(Fuse {
            session: SessionPtr(unsafe {
                NonNull::new_unchecked(mem::replace(&mut self.session, ptr::null_mut()))
            }),
            fuse_data: self.fuse_data.take().unwrap(),
            fuse_fd,
        })
    }
}

/// Wrap only the session pointer so we can catch auto-trait impl failures for cfuse_data and
/// fuse_fd.
struct SessionPtr(NonNull<libc::c_void>);

impl SessionPtr {
    #[inline]
    fn as_ptr(&self) -> sys::MutPtr {
        (self.0).as_ptr()
    }
}

unsafe impl Send for SessionPtr {}
unsafe impl Sync for SessionPtr {}

/// A mounted fuse file system.
pub struct Fuse {
    session: SessionPtr,
    fuse_data: Box<FuseData>,
    fuse_fd: PollEvented<FuseFd>,
}

// We lose these via the raw session pointer:
impl Unpin for Fuse {}

impl Drop for Fuse {
    fn drop(&mut self) {
        unsafe {
            let _ = sys::fuse_session_unmount(self.session.as_ptr());
            let _ = sys::fuse_session_destroy(self.session.as_ptr());
        }
    }
}

impl Fuse {
    pub fn builder(name: &str) -> Result<FuseSessionBuilder, Error> {
        let name = CString::new(name).map_err(|err| format_err!("bad name: {}", err))?;

        Ok(FuseSessionBuilder {
            args: vec![name],
            has_debug: false,
            operations: DEFAULT_OPERATIONS,
        })
    }
}

impl Stream for Fuse {
    type Item = io::Result<Request>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(request) = this.fuse_data.pending_requests.borrow_mut().pop_front() {
                return Poll::Ready(Some(Ok(request)));
            }

            ready!(this.fuse_fd.poll_read_ready(cx, mio::Ready::readable()))?;

            let buf: &mut sys::FuseBuf = match Arc::get_mut(&mut this.fuse_data.fbuf) {
                Some(buf) => buf,
                None => {
                    this.fuse_data.fbuf = Arc::new(sys::FuseBuf::new());
                    // we literally just did Arc::new()
                    Arc::get_mut(&mut this.fuse_data.fbuf).unwrap()
                }
            };

            let rc = unsafe { sys::fuse_session_receive_buf(this.session.as_ptr(), Some(buf)) };

            if rc == -libc::EAGAIN {
                match this.fuse_fd.clear_read_ready(cx, mio::Ready::readable()) {
                    Ok(()) => continue,
                    Err(err) => return Poll::Ready(Some(Err(err))),
                }
            } else if rc < 0 {
                return Poll::Ready(Some(Err(io::Error::from_raw_os_error(-rc))));
            }

            unsafe {
                sys::fuse_session_process_buf(this.session.as_ptr(), Some(&buf));
            }
            // and try again:
        }
    }
}
