use std::convert::TryFrom;
use std::ffi::OsStr;
use std::path::Path;
use std::{io, mem};

use anyhow::{bail, format_err, Error};
use futures::future::FutureExt;
use futures::select;
use futures::stream::TryStreamExt;
use tokio::signal::unix::{signal, SignalKind};

use proxmox_fuse::requests::{self, FuseRequest, SetTime};
use proxmox_fuse::{EntryParam, Fuse, ReplyBufState, Request};

#[macro_use]
pub mod macros;

pub mod block_file;
pub mod fs;

use fs::Fs;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut args = std::env::args_os().skip(1);

    let path = args.next().ok_or_else(|| format_err!("missing path"))?;

    let mut interrupt = signal(SignalKind::interrupt())?;
    let fuse = Fuse::builder("mytmpfs")?
        .debug()
        .enable_readdir()
        .enable_mkdir()
        .enable_rmdir()
        .enable_create()
        .enable_unlink()
        .enable_mknod()
        .enable_setattr()
        .enable_read()
        .enable_write()
        .build()?
        .mount(Path::new(&path))?;

    select! {
        res = handle_fuse(fuse).fuse() => res?,
        _ = interrupt.recv().fuse() => {
            eprintln!("interrupted");
        }
    }

    Ok(())
}

fn to_entry_param(stat: &libc::stat) -> EntryParam {
    EntryParam {
        inode: stat.st_ino,
        generation: 1,
        attr: stat.clone(),
        attr_timeout: std::f64::MAX,
        entry_timeout: std::f64::MAX,
    }
}

fn handle_io_err(
    err: io::Error,
    reply: impl FnOnce(io::Error) -> io::Result<()>,
) -> Result<(), Error> {
    // `io_bail` is used for error reporting where we return `EIO`
    if err.kind() == io::ErrorKind::Other {
        eprintln!("An IO error occured: {}", err);
    }
    reply(err)?;
    Ok(())
}

fn handle_err(err: Error, reply: impl FnOnce(io::Error) -> io::Result<()>) -> Result<(), Error> {
    match err.downcast::<io::Error>() {
        Ok(err) => handle_io_err(err, reply),
        Err(err) => {
            // `bail` (non-`io::Error`) is used for fatal errors which should actually cancel:
            eprintln!("internal error: {}", err);
            Err(err)
        }
    }
}

async fn handle_fuse(mut fuse: Fuse) -> Result<(), Error> {
    let fs = Fs::new();

    while let Some(request) = fuse.try_next().await? {
        match request {
            Request::Getattr(request) => match fs.lookup(request.inode) {
                Ok(node) => request.reply(&node.leak().stat.read().unwrap(), std::f64::MAX)?,
                Err(err) => handle_io_err(err, |err| request.io_fail(err))?,
            },
            Request::Setattr(mut request) => match handle_setattr(&fs, &mut request) {
                Ok(node) => request.reply(&node.stat.read().unwrap(), std::f64::MAX)?,
                Err(err) => handle_err(err, |err| request.io_fail(err))?,
            },
            Request::Lookup(request) => match fs.lookup_at(request.parent, &request.file_name) {
                Ok(node) => request.reply(&to_entry_param(&node.leak().stat.read().unwrap()))?,
                Err(err) => handle_io_err(err, |err| request.io_fail(err))?,
            },
            Request::Forget(request) => match fs.forget(request.inode, request.count as usize) {
                Ok(()) => request.reply(),
                Err(err) => eprintln!("error forgetting inode {}: {}", request.inode, err),
            },
            Request::Readdir(mut request) => match handle_readdir(&fs, &mut request) {
                Ok(()) => request.reply()?,
                Err(err) => handle_err(err, |err| request.io_fail(err))?,
            },
            Request::Mkdir(mut request) => {
                let reply = fs.mkdir(
                    request.parent,
                    mem::take(&mut request.dir_name),
                    request.mode,
                );
                match reply {
                    Ok(entry) => {
                        request.reply(&to_entry_param(&entry.leak().stat.read().unwrap()))?
                    }
                    Err(err) => handle_io_err(err, |err| request.io_fail(err))?,
                }
            }
            Request::Create(mut request) => {
                let reply = fs.create(
                    request.parent,
                    mem::take(&mut request.file_name),
                    request.mode,
                );
                match reply {
                    Ok(entry) => {
                        // CREATE acts as `Lookup` + `Open`
                        entry.increment_lookup();
                        request.reply(&to_entry_param(&entry.leak().stat.read().unwrap()), 0)?
                    }
                    Err(err) => handle_io_err(err, |err| request.io_fail(err))?,
                }
            }
            Request::Mknod(mut request) => {
                let reply = fs.create(
                    request.parent,
                    mem::take(&mut request.file_name),
                    request.mode,
                );
                match reply {
                    Ok(entry) => {
                        request.reply(&to_entry_param(&entry.leak().stat.read().unwrap()))?
                    }
                    Err(err) => handle_io_err(err, |err| request.io_fail(err))?,
                }
            }
            Request::Open(request) => match fs.lookup(request.inode) {
                Ok(node) => request.reply(&to_entry_param(&node.leak().stat.read().unwrap()), 0)?,
                Err(err) => handle_io_err(err, |err| request.io_fail(err))?,
            },
            Request::Release(request) => match fs.forget(request.inode, 1) {
                Ok(()) => request.reply()?,
                Err(err) => handle_io_err(err, |err| request.io_fail(err))?,
            },
            Request::Unlink(request) => {
                match fs.unlink(request.parent, &request.file_name, false) {
                    Ok(()) => request.reply()?,
                    Err(err) => handle_err(err, |err| request.io_fail(err))?,
                }
            }
            Request::Rmdir(request) => match fs.unlink(request.parent, &request.dir_name, true) {
                Ok(()) => request.reply()?,
                Err(err) => handle_err(err, |err| request.io_fail(err))?,
            },
            Request::Write(request) => {
                match fs.write(request.inode, request.data(), request.offset) {
                    Ok(()) => {
                        let len = request.data().len();
                        request.reply(len)?;
                    }
                    Err(err) => handle_io_err(err, |err| request.io_fail(err))?,
                }
            }
            Request::Read(request) => {
                // For simplicity we just limit reads to 1 MiB for now...
                let size = request.size.min(1024 * 1024);
                let mut buf = Vec::with_capacity(size);
                unsafe {
                    buf.set_len(size);
                }
                match fs.read(request.inode, &mut buf, request.offset) {
                    Ok(got) => {
                        unsafe {
                            buf.set_len(got);
                        }
                        request.reply(&buf)?;
                    }
                    Err(err) => handle_io_err(err, |err| request.io_fail(err))?,
                }
            }
            other => bail!("Got unknown request: {:?}", other),
        }
    }
    Ok(())
}

fn handle_readdir(fs: &Fs, request: &mut requests::Readdir) -> Result<(), Error> {
    let offset = match isize::try_from(request.offset) {
        Ok(offset) => offset,
        Err(_) => bail!("bad offset"),
    };

    let dir = fs.lookup(request.inode)?;

    match &dir.content {
        fs::FsContent::Dir(content) => {
            let files = content.files.read().unwrap();
            let file_count = files.len() as isize;
            let mut next = offset;
            for (name, &inode) in files.iter().skip(offset as usize) {
                next += 1;
                let inode = fs.lookup(inode)?;
                let stat = inode.stat.read().unwrap();
                match request.add_entry(&name, &stat, next)? {
                    ReplyBufState::Ok => (),
                    ReplyBufState::Full => return Ok(()),
                }
            }
            drop(files);

            if next == file_count {
                next += 1;
                let inode = fs.lookup(dir.parent)?;
                let stat = inode.stat.read().unwrap();
                match request.add_entry(OsStr::new(".."), &stat, next)? {
                    ReplyBufState::Ok => (),
                    ReplyBufState::Full => return Ok(()),
                }
            }

            if next == file_count + 1 {
                next += 1;
                match request.add_entry(OsStr::new("."), &dir.stat.read().unwrap(), next)? {
                    ReplyBufState::Ok => (),
                    ReplyBufState::Full => return Ok(()),
                }
            }

            Ok(())
        }
        _ => io_return!(libc::ENOTDIR),
    }
}

fn handle_setattr<'a>(
    fs: &'a Fs,
    request: &mut requests::Setattr,
) -> Result<fs::InodeLookup<'a>, Error> {
    use std::time::SystemTime;

    let file = fs.lookup(request.inode)?;

    let mut stat = file.stat.write().unwrap();
    let mut now = None;

    if let Some(mode) = request.mode() {
        stat.st_mode = (stat.st_mode & libc::S_IFMT) | mode
    }

    if let Some(uid) = request.uid() {
        stat.st_uid = uid;
    }

    if let Some(gid) = request.gid() {
        stat.st_gid = gid;
    }

    if let Some(size) = request.size() {
        stat.st_size = size as libc::off_t;
    }

    if let Some(time) = match request.atime() {
        Some(SetTime::Now) => {
            let new_now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap();
            now = Some(new_now);
            Some(new_now)
        }
        Some(SetTime::Time(time)) => Some(time),
        None => None,
    } {
        stat.st_atime = time.as_secs() as _;
        stat.st_atime_nsec = time.subsec_nanos() as _;
    }

    if let Some(time) = match request.mtime() {
        Some(SetTime::Now) => match now {
            Some(now) => Some(now),
            None => {
                let new_now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap();
                //now = Some(new_now);
                Some(new_now)
            }
        },
        Some(SetTime::Time(time)) => Some(time),
        None => None,
    } {
        stat.st_mtime = time.as_secs() as _;
        stat.st_mtime_nsec = time.subsec_nanos() as _;
    }

    if let Some(time) = request.ctime() {
        stat.st_ctime = time.as_secs() as _;
        stat.st_ctime_nsec = time.subsec_nanos() as _;
    }

    drop(stat);
    Ok(file)
}
