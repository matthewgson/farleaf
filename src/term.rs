//! The terminal: raw mode on /dev/tty, its size, signals, and parsing what it
//! sends us (keys, and the replies to our startup queries).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::OnceLock;

pub struct Tty {
    pub file: File,
}

/// What restore() needs, reachable from the panic hook and signal paths.
static SAVED: OnceLock<(RawFd, libc::termios)> = OnceLock::new();

/// Leaves the alternate screen with a clean slate. Safe to call twice.
pub const LEAVE: &[u8] = b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?2026l\x1b[0m\x1b[?25h\x1b[?1049l";

impl Tty {
    pub fn open() -> io::Result<Tty> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let fd = file.as_raw_fd();
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let _ = SAVED.set((fd, t));
            let mut raw = t;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Tty { file })
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn size(&self) -> Size {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        unsafe { libc::ioctl(self.fd(), libc::TIOCGWINSZ, &mut ws) };
        Size {
            cols: ws.ws_col as u32,
            rows: ws.ws_row as u32,
            px_w: ws.ws_xpixel as u32,
            px_h: ws.ws_ypixel as u32,
        }
    }
}

static ENTERED: AtomicBool = AtomicBool::new(false);

/// Call after switching to the alternate screen.
pub fn entered() {
    ENTERED.store(true, Ordering::SeqCst);
}

/// Puts the terminal back the way we found it. The alternate screen is left
/// only once: a second `d=A` would delete images in the shell's own screen.
pub fn leave() {
    if let Some((fd, t)) = SAVED.get() {
        unsafe {
            if ENTERED.swap(false, Ordering::SeqCst) {
                let _ = libc::write(*fd, LEAVE.as_ptr().cast(), LEAVE.len());
            }
            libc::tcsetattr(*fd, libc::TCSAFLUSH, t);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Size {
    pub cols: u32,
    pub rows: u32,
    pub px_w: u32,
    pub px_h: u32,
}

// --- signals and the wake pipe ------------------------------------------------
//
// Worker threads and signal handlers wake the main loop by writing one byte to
// a pipe it polls: b'W' window resized, b'T' asked to terminate, b'E' events
// waiting on the channel.

static WAKE: AtomicI32 = AtomicI32::new(-1);

pub fn wake_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK);
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    WAKE.store(fds[1], Ordering::Relaxed);
    Ok((fds[0], fds[1]))
}

pub fn wake(fd: RawFd, why: u8) {
    unsafe { libc::write(fd, (&why as *const u8).cast(), 1) };
}

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = WAKE.load(Ordering::Relaxed);
    if fd >= 0 {
        wake(fd, if sig == libc::SIGWINCH { b'W' } else { b'T' });
    }
}

pub fn install_signals() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGWINCH, libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Waits until one of `fds` is readable or `timeout_ms` passes. Returns which
/// were readable (none on timeout or when a signal cut the wait short).
///
/// select(), not poll(): macOS's poll() does not work on /dev/tty.
pub fn poll(fds: &[RawFd], timeout_ms: i32) -> Vec<bool> {
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        for &fd in fds {
            libc::FD_SET(fd, &mut set);
        }
        let max = fds.iter().copied().max().unwrap_or(0);
        let mut tv = libc::timeval {
            tv_sec: (timeout_ms / 1000) as _,
            tv_usec: ((timeout_ms % 1000) * 1000) as _,
        };
        let n = libc::select(max + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv);
        if n <= 0 {
            return vec![false; fds.len()];
        }
        fds.iter().map(|&fd| libc::FD_ISSET(fd, &set)).collect()
    }
}

/// Some(0) is end of file; None means nothing to read right now (EAGAIN) or
/// a signal interrupted the read.
pub fn read_some(fd: RawFd, buf: &mut [u8]) -> Option<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    (n >= 0).then_some(n as usize)
}

pub fn write_all(file: &mut File, data: &[u8]) -> io::Result<()> {
    file.write_all(data)?;
    file.flush()
}

// --- input ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Ctrl plus a letter, as the lowercase letter.
    Ctrl(char),
    Enter,
    Esc,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    /// A Kitty graphics reply: image id, and the error (None means OK).
    Graphics(u32, Option<String>),
    /// XTVERSION reply: the terminal's name and version ("ghostty 1.3.1";
    /// herdr's panes say "libghostty").
    Version(String),
    /// CSI 16 t reply: cell width and height in pixels.
    CellSize(u32, u32),
    /// The reply to DA1, which every terminal sends: our "end of replies" marker.
    DeviceAttributes,
}

#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
}

impl Parser {
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// The next complete input. With `flush`, an incomplete escape sequence is
    /// given up on and a lone ESC becomes the Esc key (call it after a short
    /// quiet period).
    pub fn next(&mut self, flush: bool) -> Option<Input> {
        loop {
            if self.buf.is_empty() {
                return None;
            }
            match parse(&self.buf) {
                Parsed::Done(n, item) => {
                    self.buf.drain(..n);
                    if let Some(item) = item {
                        return Some(item);
                    }
                }
                Parsed::Incomplete if flush => {
                    // Treat the ESC as a key; the rest is parsed fresh.
                    self.buf.drain(..1);
                    return Some(Input::Key(Key::Esc));
                }
                Parsed::Incomplete => return None,
            }
        }
    }
}

enum Parsed {
    Done(usize, Option<Input>),
    Incomplete,
}

fn parse(b: &[u8]) -> Parsed {
    use Parsed::*;
    let key = |n, k| Done(n, Some(Input::Key(k)));
    match b[0] {
        0x1b => {
            let Some(&next) = b.get(1) else { return Incomplete };
            match next {
                b'[' => csi(b),
                b'O' => match b.get(2) {
                    None => Incomplete,
                    Some(c) => Done(
                        3,
                        match c {
                            b'A' => Some(Input::Key(Key::Up)),
                            b'B' => Some(Input::Key(Key::Down)),
                            b'C' => Some(Input::Key(Key::Right)),
                            b'D' => Some(Input::Key(Key::Left)),
                            b'H' => Some(Input::Key(Key::Home)),
                            b'F' => Some(Input::Key(Key::End)),
                            _ => None,
                        },
                    ),
                },
                b'_' | b'P' | b']' => string_seq(b),
                // ESC followed by anything else is Alt+key, which we have no
                // use for: report Esc and let the key through on its own.
                _ => key(1, Key::Esc),
            }
        }
        b'\r' | b'\n' => key(1, Key::Enter),
        b'\t' => key(1, Key::Tab),
        0x7f | 0x08 => key(1, Key::Backspace),
        c @ 0x01..=0x1a => key(1, Key::Ctrl((b'a' + c - 1) as char)),
        c if c < 0x20 => Done(1, None),
        c => {
            let len = match c {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Done(1, None),
            };
            if b.len() < len {
                return Incomplete;
            }
            match std::str::from_utf8(&b[..len]).ok().and_then(|s| s.chars().next()) {
                Some(ch) => key(len, Key::Char(ch)),
                None => Done(1, None),
            }
        }
    }
}

fn csi(b: &[u8]) -> Parsed {
    let Some(end) = b[2..].iter().position(|&c| (0x40..=0x7e).contains(&c)) else {
        return if b.len() > 64 {
            Parsed::Done(b.len(), None)
        } else {
            Parsed::Incomplete
        };
    };
    let end = end + 2;
    let params = &b[2..end];
    let fin = b[end];
    let nums: Vec<u32> = params
        .split(|&c| c == b';')
        .map(|p| {
            p.iter()
                .filter(|c| c.is_ascii_digit())
                .fold(0u32, |a, &c| a.saturating_mul(10).saturating_add((c - b'0') as u32))
        })
        .collect();
    let k = |k| Some(Input::Key(k));
    let item = match fin {
        b'A' => k(Key::Up),
        b'B' => k(Key::Down),
        b'C' => k(Key::Right),
        b'D' => k(Key::Left),
        b'H' => k(Key::Home),
        b'F' => k(Key::End),
        b'~' => match nums.first() {
            Some(5) => k(Key::PageUp),
            Some(6) => k(Key::PageDown),
            Some(1) | Some(7) => k(Key::Home),
            Some(4) | Some(8) => k(Key::End),
            _ => None,
        },
        b'c' if params.first() == Some(&b'?') => Some(Input::DeviceAttributes),
        b't' if nums.first() == Some(&6) && nums.len() >= 3 => Some(Input::CellSize(nums[2], nums[1])),
        _ => None,
    };
    Parsed::Done(end + 1, item)
}

/// APC / DCS / OSC: runs to ST (ESC \) or BEL.
fn string_seq(b: &[u8]) -> Parsed {
    let mut i = 2;
    while i < b.len() {
        if b[i] == 0x07 {
            return Parsed::Done(i + 1, graphics_reply(b, i));
        }
        if b[i] == 0x1b {
            if i + 1 >= b.len() {
                return Parsed::Incomplete;
            }
            if b[i + 1] == b'\\' {
                return Parsed::Done(i + 2, graphics_reply(b, i));
            }
        }
        i += 1;
    }
    Parsed::Incomplete
}

fn graphics_reply(b: &[u8], end: usize) -> Option<Input> {
    if b[1] == b'P' {
        let v = b.get(2..end)?.strip_prefix(b">|")?;
        return Some(Input::Version(String::from_utf8_lossy(v).into_owned()));
    }
    let body = b.get(2..end)?.strip_prefix(b"G")?;
    if b[1] != b'_' {
        return None;
    }
    let (keys, msg) = match body.iter().position(|&c| c == b';') {
        Some(p) => (&body[..p], &body[p + 1..]),
        None => (body, &b""[..]),
    };
    let id = keys
        .split(|&c| c == b',')
        .find_map(|kv| kv.strip_prefix(b"i="))
        .and_then(|v| std::str::from_utf8(v).ok()?.parse().ok())?;
    Some(Input::Graphics(
        id,
        (msg != b"OK").then(|| String::from_utf8_lossy(msg).into_owned()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(bytes: &[u8]) -> Vec<Input> {
        let mut p = Parser::default();
        p.feed(bytes);
        std::iter::from_fn(|| p.next(true)).collect()
    }

    #[test]
    fn keys_and_replies() {
        assert_eq!(
            all(b"j\x04\x1b[6~\x1b_Gi=1;OK\x1b\\\x1b_Gi=2;ENOENT:x\x1b\\\x1b[6;42;19t\x1bP>|libghostty\x1b\\\x1b[?62;22cq"),
            vec![
                Input::Key(Key::Char('j')),
                Input::Key(Key::Ctrl('d')),
                Input::Key(Key::PageDown),
                Input::Graphics(1, None),
                Input::Graphics(2, Some("ENOENT:x".into())),
                Input::CellSize(19, 42),
                Input::Version("libghostty".into()),
                Input::DeviceAttributes,
                Input::Key(Key::Char('q')),
            ]
        );
    }

    #[test]
    fn split_sequence_waits_then_flushes() {
        let mut p = Parser::default();
        p.feed(b"\x1b[");
        assert_eq!(p.next(false), None);
        p.feed(b"B");
        assert_eq!(p.next(false), Some(Input::Key(Key::Down)));
        p.feed(b"\x1b");
        assert_eq!(p.next(false), None);
        assert_eq!(p.next(true), Some(Input::Key(Key::Esc)));
    }
}
