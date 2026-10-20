//! Cancellable Unix PTY handles. A background job can keep the slave open
//! after the shell exits, so waiting for read/write EOF is not a shutdown path.
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use portable_pty::MasterPty;

type Reader = Box<dyn Read + Send>;
type Writer = Box<dyn Write + Send>;

#[cfg(unix)]
pub(super) fn open(pty: &dyn MasterPty, closed: Arc<AtomicBool>) -> Result<(Reader, Writer)> {
    use std::os::fd::{AsRawFd, BorrowedFd};

    let fd = pty
        .as_raw_fd()
        .ok_or_else(|| anyhow::anyhow!("PTY has no file descriptor"))?;
    // The borrowed descriptor remains owned by `pty`; both workers receive
    // owned duplicates so the waiter may safely drop the original master.
    let file = std::fs::File::from(unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?);
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((
        Box::new(PtyIo {
            file: file.try_clone()?,
            closed: closed.clone(),
        }),
        Box::new(PtyIo { file, closed }),
    ))
}

#[cfg(not(unix))]
pub(super) fn open(pty: &dyn MasterPty, _closed: Arc<AtomicBool>) -> Result<(Reader, Writer)> {
    // ConPTY closes the pipes when its master is dropped by the waiter.
    Ok((pty.try_clone_reader()?, pty.take_writer()?))
}

#[cfg(unix)]
struct PtyIo {
    file: std::fs::File,
    closed: Arc<AtomicBool>,
}

#[cfg(unix)]
impl PtyIo {
    fn perform<T>(
        &mut self,
        events: libc::c_short,
        mut operation: impl FnMut(&mut std::fs::File) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::Ordering;

        loop {
            if self.closed.load(Ordering::Acquire) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Terminal closed",
                ));
            }
            let mut descriptor = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events,
                revents: 0,
            };
            // A finite readiness wait also interrupts a full stdin queue.
            // Nonblocking IO prevents a readiness race from trapping a worker.
            let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                continue;
            }
            match operation(&mut self.file) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
}

#[cfg(unix)]
impl Read for PtyIo {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.perform(libc::POLLIN, |file| file.read(bytes))
    }
}

#[cfg(unix)]
impl Write for PtyIo {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.perform(libc::POLLOUT, |file| file.write(bytes))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
