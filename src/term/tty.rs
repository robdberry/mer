//! The controlling terminal, independent of stdin and stdout.
//!
//! Probe replies and key presses are always read from `/dev/tty`, so `cat file | mer -` keeps
//! working when stdin is a pipe.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use rustix::event::{FdSetElement, Timespec, fd_set_insert, fd_set_num_elements, select};
use rustix::termios::{
    self, InputModes, LocalModes, OptionalActions, SpecialCodeIndex, Termios, Winsize,
};

/// A handle to the controlling terminal. Clones share one open file.
#[derive(Clone)]
pub struct Tty {
    file: Arc<File>,
}

impl Tty {
    pub fn open() -> io::Result<Tty> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        Ok(Tty {
            file: Arc::new(file),
        })
    }

    pub fn winsize(&self) -> io::Result<Winsize> {
        Ok(termios::tcgetwinsize(&*self.file)?)
    }

    /// Turns off line buffering and echo until the guard is dropped. With `keys`, signal keys
    /// and flow control arrive as bytes too, for modes that handle their own input.
    pub fn raw(&self, keys: bool) -> io::Result<RawMode> {
        let saved = termios::tcgetattr(&*self.file)?;
        let mut raw = saved.clone();
        raw.local_modes.remove(LocalModes::ICANON | LocalModes::ECHO);
        if keys {
            raw.local_modes.remove(LocalModes::ISIG | LocalModes::IEXTEN);
            raw.input_modes.remove(InputModes::IXON | InputModes::ICRNL);
        }
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        termios::tcsetattr(&*self.file, OptionalActions::Now, &raw)?;
        Ok(RawMode {
            file: Arc::clone(&self.file),
            saved,
        })
    }

    /// Waits up to `timeout` for input and reads what is there. Returns `None` on timeout and
    /// `Some(0)` at end of file.
    pub fn read_timeout(&self, buf: &mut [u8], timeout: Duration) -> io::Result<Option<usize>> {
        // `select` rather than `poll`: on macOS, `poll` reports terminal devices as ready
        // (POLLNVAL) without data, and the read that follows blocks.
        let fd = self.file.as_raw_fd();
        let mut readable = vec![FdSetElement::default(); fd_set_num_elements(1, fd + 1)];
        fd_set_insert(&mut readable, fd);
        let timeout = Timespec {
            tv_sec: timeout.as_secs() as _,
            tv_nsec: timeout.subsec_nanos() as _,
        };
        // SAFETY: the only fd in the set belongs to `self.file`, which is open for this call.
        match unsafe { select(fd + 1, Some(&mut readable), None, None, Some(&timeout)) } {
            Ok(0) | Err(rustix::io::Errno::INTR) => return Ok(None),
            Ok(_) => {}
            Err(err) => return Err(err.into()),
        }
        (&*self.file).read(buf).map(Some)
    }

    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut file = &*self.file;
        file.write_all(bytes)?;
        file.flush()
    }
}

/// Restores the terminal settings captured by [`Tty::raw`] when dropped.
pub struct RawMode {
    file: Arc<File>,
    saved: Termios,
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(&*self.file, OptionalActions::Now, &self.saved);
    }
}
