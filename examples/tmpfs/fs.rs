//! The tmpfs.

use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::ffi::{OsStr, OsString};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use std::{io, mem};

use anyhow::Error;

use crate::block_file::BlockFile;

fn now() -> Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
}

fn new_stat(inode: u64, mode: libc::mode_t, uid: libc::uid_t, gid: libc::gid_t) -> libc::stat {
    let mut stat: libc::stat = unsafe { mem::zeroed() };
    stat.st_ino = inode;
    stat.st_mode = mode;
    stat.st_uid = uid;
    stat.st_gid = gid;
    let now = now();
    stat.st_mtime = now.as_secs() as i64;
    stat.st_mtime_nsec = i64::from(now.subsec_nanos());
    stat.st_atime = stat.st_mtime;
    stat.st_atime_nsec = stat.st_mtime_nsec;
    stat.st_ctime = stat.st_mtime;
    stat.st_ctime_nsec = stat.st_mtime_nsec;
    stat
}

fn new_dir_stat(inode: u64) -> libc::stat {
    new_stat(inode, 0o755 | libc::S_IFDIR, 0, 0)
}

pub struct Fs {
    entries: RwLock<Vec<Option<Box<FsEntry>>>>,
    free_inodes: Mutex<Vec<u64>>,
}

impl Fs {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(vec![
                None,
                Some(Box::new(FsEntry {
                    inode: proxmox_fuse::ROOT_ID,
                    parent: proxmox_fuse::ROOT_ID,
                    lookups: AtomicUsize::new(1),
                    links: AtomicUsize::new(1),
                    stat: RwLock::new(new_dir_stat(1)),
                    content: FsContent::Dir(Dir::new()),
                })),
            ]),
            free_inodes: Mutex::new(Vec::new()),
        }
    }

    pub fn lookup(&self, inode: u64) -> io::Result<InodeLookup> {
        let inode = usize::try_from(inode).map_err(|_| {
            // we could have never created such an inode for the kernel...
            io_format_err!("kernel accessed unexpected too-large inode: {}", inode)
        })?;
        match self.entries.read().unwrap().get(inode) {
            Some(Some(entry)) => {
                let entry: &FsEntry = entry;
                let entry = entry as *const FsEntry;
                Ok(InodeLookup::new(self, unsafe { &*entry }))
            }
            // This inode has been deleted...
            Some(None) => io_return!(libc::ENOENT),
            // This inode has never been advertised to the kernel:
            None => io_bail!("kernel looked up never-advertised inode: {}", inode),
        }
    }

    pub fn lookup_at(&self, inode: u64, name: &OsStr) -> io::Result<InodeLookup> {
        let node = self.lookup(inode)?;

        if name == OsStr::new(".") {
            return Ok(node);
        }

        match &node.content {
            FsContent::Dir(dir) => {
                if name == OsStr::new("..") {
                    return self.lookup(node.parent);
                }

                match dir.files.read().unwrap().get(name) {
                    Some(inode) => self.lookup(*inode),
                    None => io_return!(libc::ENOENT),
                }
            }
            _ => io_return!(libc::ENOTDIR),
        }
    }

    unsafe fn forget_entry(&self, entry: &FsEntry, nlookup: usize) {
        if entry.lookups.fetch_sub(nlookup, Ordering::AcqRel) > 1 {
            // there were still more lookups present
            return;
        }

        // lookup count dropped to zero, check the hard links:
        if entry.links.load(Ordering::Acquire) != 0 {
            return;
        }

        let inode = entry.inode;

        entry.on_drop(self);
        drop(entry);

        // no lookups, no hard links, delete:
        eprintln!("Deleting inode {}", inode);
        let mut entries_lock = self.entries.write().unwrap();
        entries_lock[inode as usize] = None;
        drop(entries_lock);
        self.free_inodes.lock().unwrap().push(inode);
    }

    pub fn forget(&self, inode: u64, nlookup: usize) -> io::Result<()> {
        let entries_lock = self.entries.read().unwrap();
        match entries_lock.get(inode as usize).as_ref() {
            Some(Some(entry)) => {
                unsafe {
                    let entry = entry.as_ref() as *const FsEntry;
                    drop(entries_lock);
                    self.forget_entry(&*entry, nlookup);
                }
                Ok(())
            }
            Some(None) => io_return!(libc::ENOENT),
            None => io_bail!("tried to forget a never-looked-up inode"),
        }
    }

    fn do_create(
        &self,
        parent: u64,
        name: OsString,
        mode: libc::mode_t,
        fs_content: FsContent,
    ) -> io::Result<InodeLookup> {
        use std::collections::btree_map::Entry::*;
        let parent_dir = self.lookup(parent)?;

        match &parent_dir.content {
            FsContent::Dir(content) => {
                let mut content = content.files.write().unwrap();
                match content.entry(name) {
                    Occupied(_) => io_return!(libc::EEXIST),
                    Vacant(vacancy) => {
                        let mut stat = new_stat(0, mode, 0, 0);

                        // create an inode and put a write-lock the entry list
                        let inode = self.free_inodes.lock().unwrap().pop();
                        let mut entry_lock = self.entries.write().unwrap();
                        let inode = match inode {
                            Some(inode) => inode,
                            None => {
                                let inode = entry_lock.len();
                                entry_lock.push(None);
                                inode as u64
                            }
                        };

                        // create the directory
                        stat.st_ino = inode;
                        let dir = Box::new(FsEntry {
                            inode,
                            parent,
                            lookups: AtomicUsize::new(0),
                            links: AtomicUsize::new(1),
                            stat: RwLock::new(stat),
                            content: fs_content,
                        });

                        let ptr = &*dir as *const FsEntry;

                        // Insert into the file system:
                        entry_lock[inode as usize] = Some(dir);
                        // Hardlink the inode into the directory
                        vacancy.insert(inode);

                        Ok(InodeLookup::new(self, unsafe { &*ptr }))
                    }
                }
            }
            _ => io_return!(libc::ENOTDIR),
        }
    }

    pub fn mkdir(
        &self,
        parent: u64,
        name: OsString,
        mode: libc::mode_t,
    ) -> io::Result<InodeLookup> {
        self.do_create(
            parent,
            name,
            mode | libc::S_IFDIR,
            FsContent::Dir(Dir::new()),
        )
    }

    pub fn create(
        &self,
        parent: u64,
        name: OsString,
        mode: libc::mode_t,
    ) -> io::Result<InodeLookup> {
        self.do_create(
            parent,
            name,
            mode | libc::S_IFREG,
            FsContent::File(File::new()),
        )
    }

    pub fn unlink(&self, parent: u64, name: &OsStr, is_rmdir: bool) -> Result<(), Error> {
        let parent_dir = self.lookup(parent)?;
        let entry = match &parent_dir.content {
            FsContent::Dir(content) => {
                let mut content = content.files.write().unwrap();

                // FIXME: once BTreeMap::remove_entry is stable, use this to avoid cloning `name`.
                let inode = match content.remove(name) {
                    Some(entry) => entry,
                    None => io_return!(libc::ENOENT),
                };

                let entry = self.lookup(inode)?;
                match &entry.content {
                    FsContent::Dir(_) if !is_rmdir => {
                        content.insert(name.to_owned(), inode);
                        io_return!(libc::EISDIR);
                    }
                    FsContent::Dir(dir) if !dir.is_empty() => {
                        content.insert(name.to_owned(), inode);
                        io_return!(libc::ENOTEMPTY);
                    }
                    FsContent::Dir(_) => (),
                    _ if is_rmdir => {
                        content.insert(name.to_owned(), inode);
                        io_return!(libc::ENOTDIR);
                    }
                    _ => (),
                }

                entry
            }
            _ => io_return!(libc::ENOTDIR),
        };

        entry.links.fetch_sub(1, Ordering::AcqRel);

        Ok(())
    }

    pub fn write(&self, inode: u64, data: &[u8], offset: u64) -> io::Result<()> {
        let node = self.lookup(inode)?;
        match &node.content {
            FsContent::File(file) => {
                let new_size = file.write(data, offset)?;
                node.stat.write().unwrap().st_size = new_size as libc::off_t;
                Ok(())
            }
            _ => io_return!(libc::EBADF),
        }
    }

    pub fn read(&self, inode: u64, data: &mut [u8], offset: u64) -> io::Result<usize> {
        let node = self.lookup(inode)?;
        match &node.content {
            FsContent::File(file) => file.read(data, offset),
            _ => io_return!(libc::EBADF),
        }
    }
}

pub struct InodeLookup<'a> {
    fs: &'a Fs,
    entry: Option<&'a FsEntry>,
}

impl<'a> Drop for InodeLookup<'a> {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            unsafe {
                self.fs.forget_entry(entry, 1);
            }
        }
    }
}

impl<'a> InodeLookup<'a> {
    fn new(fs: &'a Fs, entry: &'a FsEntry) -> InodeLookup<'a> {
        entry.lookups.fetch_add(1, Ordering::AcqRel);
        Self {
            fs,
            entry: Some(entry),
        }
    }

    pub fn leak(mut self) -> &'a FsEntry {
        self.entry.take().unwrap()
    }

    pub fn increment_lookup(&self) {
        self.entry.unwrap().lookups.fetch_add(1, Ordering::AcqRel);
    }
}

impl<'a> std::ops::Deref for InodeLookup<'a> {
    type Target = FsEntry;

    fn deref(&self) -> &Self::Target {
        self.entry.clone().unwrap()
    }
}

pub struct FsEntry {
    pub inode: u64,
    pub parent: u64,
    pub lookups: AtomicUsize,
    pub links: AtomicUsize,
    pub stat: RwLock<libc::stat>,
    pub content: FsContent,
}

impl FsEntry {
    fn try_add_link(&self) -> io::Result<()> {
        loop {
            let links = self.links.load(Ordering::Acquire);
            if links == 0 {
                eprintln!("Tried to increase a hardlink count of 0");
                io_return!(libc::ENOENT);
            }
            if self
                .links
                .compare_exchange(links, links + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn on_drop(&self, fs: &Fs) {
        if let FsContent::Dir(dir) = &self.content {
            if let Err(err) = dir.on_drop(fs) {
                eprintln!("error cleaning out directory: {}", err);
            }
        }
    }
}

pub enum FsContent {
    Dir(Dir),
    File(File),
}

pub struct Dir {
    pub files: RwLock<BTreeMap<OsString, u64>>,
}

impl Dir {
    fn new() -> Self {
        Self {
            files: RwLock::new(BTreeMap::new()),
        }
    }

    fn is_empty(&self) -> bool {
        self.files.read().unwrap().is_empty()
    }

    fn on_drop(&self, fs: &Fs) -> io::Result<()> {
        let files = mem::take(&mut *self.files.write().unwrap());

        for inode in files.values() {
            fs.lookup(*inode)?.links.fetch_sub(1, Ordering::AcqRel);
        }

        Ok(())
    }
}

pub struct File {
    data: RwLock<BlockFile>,
}

impl File {
    fn new() -> Self {
        Self {
            data: RwLock::new(BlockFile::new(4096)),
        }
    }

    /// Returns the new absolute file size, so we can update the stat member.
    fn write(&self, data: &[u8], offset: u64) -> io::Result<u64> {
        let mut content = self.data.write().unwrap();
        content.write(data, offset)?;
        Ok(content.size())
    }

    /// Returns the amount of bytes actually read.
    fn read(&self, data: &mut [u8], offset: u64) -> io::Result<usize> {
        self.data.read().unwrap().read(data, offset)
    }
}
