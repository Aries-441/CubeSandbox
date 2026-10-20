// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    fs: PassthroughFs,
    path: PathBuf,
    ctx: Context,
}

impl Fixture {
    fn new(xattr: bool) -> Self {
        // PassthroughFs changes CWD for proc-fd xattr calls and sets the umask.
        // Keep these changes local to this test thread, as in the daemon workers.
        // Safe because unshare does not access userspace memory.
        assert_eq!(unsafe { libc::unshare(libc::CLONE_FS) }, 0);

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "virtiofsd-xattr-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let fs = PassthroughFs::new(Config {
            root_dir: path.to_str().unwrap().to_owned(),
            xattr,
            ..Default::default()
        })
        .unwrap();
        fs.open_root_node().unwrap();
        // Safe because these calls do not access userspace memory.
        let ctx = unsafe {
            Context {
                uid: libc::getuid(),
                gid: libc::getgid(),
                pid: libc::getpid(),
            }
        };
        Self { fs, path, ctx }
    }

    fn inode(&self, name: &str) -> Inode {
        self.fs
            .lookup(self.ctx, fuse::ROOT_ID, &CString::new(name).unwrap(), None)
            .unwrap()
            .inode
    }

    fn set_xattr(&self, inode: Inode, name: &CStr, value: &[u8]) {
        self.fs
            .setxattr(self.ctx, inode, name, value, 0, SetxattrFlags::empty())
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

#[test]
fn getxattr_does_not_open_regular_file() {
    let fixture = Fixture::new(true);
    fs::write(fixture.path.join("file"), b"hello world").unwrap();
    let inode = fixture.inode("file");
    let name = CString::new("user.virtiofsd_test").unwrap();
    fixture.set_xattr(inode, &name, b"value");

    // Watch after setup: the metadata reads below must not generate data-file
    // open/close events. An extra open/close can undo O_TRUNC on s3fs (#1991).
    // Safe because inotify_init1 does not access userspace memory.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(fd >= 0);
    // Safe because fd is a newly allocated, owned descriptor.
    let mut watch = unsafe { File::from_raw_fd(fd) };
    let path = CString::new(fixture.path.join("file").as_os_str().as_bytes()).unwrap();
    // Safe because path is NUL-terminated and the return value is checked.
    let wd = unsafe {
        libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_OPEN | libc::IN_CLOSE_NOWRITE)
    };
    assert!(wd >= 0);

    assert!(matches!(
        fixture.fs.getxattr(fixture.ctx, inode, &name, 0).unwrap(),
        GetxattrReply::Count(5)
    ));
    assert!(matches!(
        fixture.fs.getxattr(fixture.ctx, inode, &name, 5).unwrap(),
        GetxattrReply::Value(value) if value == b"value"
    ));
    let mut events = [0; 1024];
    let err = watch
        .read(&mut events)
        .expect_err("getxattr must not open or close the data file");
    assert_eq!(err.raw_os_error(), Some(libc::EAGAIN));
}

#[test]
fn getxattr_replies_for_files_and_directories() {
    let fixture = Fixture::new(true);
    fs::write(fixture.path.join("file"), b"data").unwrap();
    fs::create_dir(fixture.path.join("dir")).unwrap();
    let name = CString::new("user.virtiofsd_test").unwrap();
    let missing = CString::new("user.missing").unwrap();
    let expected = b"\0hi\xff";
    for entry in ["file", "dir"] {
        let inode = fixture.inode(entry);
        fixture.set_xattr(inode, &name, expected);
        assert!(matches!(
            fixture.fs.getxattr(fixture.ctx, inode, &name, 0).unwrap(),
            GetxattrReply::Count(4)
        ));
        assert!(matches!(
            fixture.fs.getxattr(fixture.ctx, inode, &name, 16).unwrap(),
            GetxattrReply::Value(value) if value == expected
        ));
        let err = fixture
            .fs
            .getxattr(fixture.ctx, inode, &name, 1)
            .err()
            .unwrap();
        assert_eq!(err.raw_os_error(), Some(libc::ERANGE));
        let err = fixture
            .fs
            .getxattr(fixture.ctx, inode, &missing, 0)
            .err()
            .unwrap();
        assert_eq!(err.raw_os_error(), Some(libc::ENODATA));
    }
}

#[test]
fn getxattr_uses_inode_after_unlink() {
    let fixture = Fixture::new(true);
    fs::write(fixture.path.join("file"), b"data").unwrap();
    let inode = fixture.inode("file");
    let name = CString::new("user.virtiofsd_test").unwrap();
    fixture.set_xattr(inode, &name, b"original");
    fs::remove_file(fixture.path.join("file")).unwrap();
    assert!(matches!(
        fixture.fs.getxattr(fixture.ctx, inode, &name, 8).unwrap(),
        GetxattrReply::Value(value) if value == b"original"
    ));
}

#[test]
fn getxattr_disabled_returns_enosys() {
    let fixture = Fixture::new(false);
    let name = CString::new("user.virtiofsd_test").unwrap();
    let err = fixture
        .fs
        .getxattr(fixture.ctx, fuse::ROOT_ID, &name, 0)
        .err()
        .unwrap();
    assert_eq!(err.raw_os_error(), Some(libc::ENOSYS));
}
