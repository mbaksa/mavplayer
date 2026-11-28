//! Wake the existing UI tick only when transfer directories change. On Linux
//! the kernel blocks a small watcher thread; no directory scan or syscall is
//! needed in the idle UI tick. Other platforms use the existing timer fallback.
use std::path::Path;

pub(crate) struct TransferWatch {
    #[cfg(target_os = "linux")]
    native: Option<linux::Watch>,
    initial: bool,
}

impl TransferWatch {
    pub(crate) fn new(root: &Path) -> Self {
        #[cfg(not(target_os = "linux"))]
        let _ = root;
        Self {
            #[cfg(target_os = "linux")]
            native: match linux::Watch::new(root) {
                Ok(watch) => Some(watch),
                Err(err) => {
                    log::warn!("Transfer directory watcher unavailable: {err}");
                    None
                }
            },
            initial: true,
        }
    }

    pub(crate) fn ready(&mut self, fallback_due: bool) -> bool {
        let initial = std::mem::take(&mut self.initial);
        #[cfg(target_os = "linux")]
        if let Some(watch) = &self.native {
            if watch.failed() {
                self.native = None;
                return true; // Reconcile after losing the watch; then use fallback.
            }
            return watch.take_changed() || initial;
        }
        initial || fallback_due
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::fs::{self, File};
    use std::io::{self, Read};
    use std::net::Shutdown;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread::{self, JoinHandle};

    pub(super) struct Watch {
        changed: Arc<AtomicBool>,
        failed: Arc<AtomicBool>,
        cancel: UnixStream,
        worker: Option<JoinHandle<()>>,
    }

    impl Watch {
        pub(super) fn new(root: &Path) -> io::Result<Self> {
            let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: inotify_init1 returned a new, exclusively owned descriptor.
            let file = unsafe { File::from_raw_fd(fd) };
            for name in ["inbox", "outbox"] {
                let directory = root.join(name);
                fs::create_dir_all(&directory)?;
                let path = CString::new(directory.as_os_str().as_bytes())
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid transfer directory"))?;
                let mask = libc::IN_MOVED_TO
                    | libc::IN_CLOSE_WRITE
                    | libc::IN_DELETE
                    | libc::IN_MOVED_FROM
                    | libc::IN_DELETE_SELF
                    | libc::IN_MOVE_SELF
                    | libc::IN_ONLYDIR;
                if unsafe { libc::inotify_add_watch(file.as_raw_fd(), path.as_ptr(), mask) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            let (cancel, stop) = UnixStream::pair()?;
            let changed = Arc::new(AtomicBool::new(false));
            let failed = Arc::new(AtomicBool::new(false));
            let (signal, error) = (changed.clone(), failed.clone());
            let worker = thread::Builder::new().name("mav-transfer-watch".into()).spawn(move || {
                if let Err(err) = run(file, stop, &signal) {
                    log::warn!("Transfer directory watcher stopped: {err}");
                    error.store(true, Ordering::Release);
                }
            })?;
            Ok(Self { changed, failed, cancel, worker: Some(worker) })
        }

        pub(super) fn take_changed(&self) -> bool {
            self.changed.swap(false, Ordering::AcqRel)
        }

        pub(super) fn failed(&self) -> bool {
            self.failed.load(Ordering::Acquire)
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            let _ = self.cancel.shutdown(Shutdown::Both);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn run(mut file: File, stop: UnixStream, changed: &AtomicBool) -> io::Result<()> {
        let mut fds = [
            libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let mut buffer = [0u8; 16384];
        loop {
            // The stop socket wakes poll immediately on collection switch or exit.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if fds[1].revents != 0 {
                return Ok(());
            }
            if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::other("Transfer directory watch lost"));
            }
            if fds[0].revents & libc::POLLIN == 0 {
                continue;
            }
            match file.read(&mut buffer) {
                Ok(0) => return Err(io::Error::other("Transfer directory watch closed")),
                Ok(n) => {
                    // inotify_event contains four 32-bit fields followed by len bytes.
                    // Decode bytewise to avoid unaligned pointer access.
                    let mut offset = 0;
                    while offset + 16 <= n {
                        let mask = u32::from_ne_bytes(buffer[offset + 4..offset + 8].try_into().unwrap());
                        let len = u32::from_ne_bytes(buffer[offset + 12..offset + 16].try_into().unwrap()) as usize;
                        if mask & (libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_UNMOUNT) != 0
                        {
                            return Err(io::Error::other("A transfer directory was removed or moved"));
                        }
                        // Overflow also requests a complete reconciliation scan.
                        changed.store(true, Ordering::Release);
                        offset += 16 + len;
                    }
                }
                Err(err) if matches!(err.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                Err(err) => return Err(err),
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        fs, thread,
        time::{Duration, Instant},
    };

    #[test]
    fn transfer_watch_wakes_on_inbox_and_outbox_publication() {
        let root = std::env::temp_dir().join(format!("mav-transfer-watch-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let mut watch = TransferWatch::new(&root);
        assert!(watch.native.is_some(), "Native Linux watcher unavailable");
        assert!(watch.ready(false));
        assert!(!watch.ready(true)); // The fallback timer alone cannot trigger a scan.
        for (name, filename) in [("inbox", "request.move"), ("outbox", "request.move-ack")] {
            fs::write(root.join(name).join(filename), b"request").unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !watch.ready(false) {
                assert!(Instant::now() < deadline, "No filesystem notification for {name}");
                thread::sleep(Duration::from_millis(5));
            }
        }
        drop(watch);
        fs::remove_dir_all(root).unwrap();
    }
}
