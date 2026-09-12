//! The controlling terminal, independent of stdin and stdout.
//!
//! Probe replies and key presses are always read from `/dev/tty`, so `cat file | mer -` keeps
//! working when stdin is a pipe.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
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
        let timeout = Timespec {
            tv_sec: timeout.as_secs() as _,
            tv_nsec: timeout.subsec_nanos() as _,
        };
        let mut fds = [PollFd::new(&*self.file, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
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
