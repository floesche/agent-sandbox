//! Bounded, cancellable byte-stream forwarding. No ancillary FD/credential transfer.
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::Connection;

pub enum Stream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    fn nonblocking(&self) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.set_nonblocking(true),
            Self::Tcp(s) => s.set_nonblocking(true),
        }
    }
    fn shutdown_write(&self) {
        let _ = match self {
            Self::Unix(s) => s.shutdown(Shutdown::Write),
            Self::Tcp(s) => s.shutdown(Shutdown::Write),
        };
    }
}
impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(s) => s.read(buffer),
            Self::Tcp(s) => s.read(buffer),
        }
    }
}
impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(s) => s.write(buffer),
            Self::Tcp(s) => s.write(buffer),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn connect(connection: &Connection) -> io::Result<Stream> {
    if let Some(address) = connection.host_tcp {
        return TcpStream::connect_timeout(&address, Duration::from_secs(1)).map(Stream::Tcp);
    }
    let path = connection.host_unix.as_ref().expect("validated endpoint");
    let bytes = path.as_os_str().as_encoded_bytes();
    // Nonblocking connect also bounds a Unix listener whose backlog is full.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of_val(&address) as _;
    }
    if bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix path too long",
        ));
    }
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as _;
    }
    #[cfg(target_os = "linux")]
    let flags = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = libc::SOCK_STREAM;
    let fd = unsafe { libc::socket(libc::AF_UNIX, flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Own immediately so every error closes the descriptor.
    let socket = unsafe { UnixStream::from_raw_fd(fd) };
    #[cfg(not(target_os = "linux"))]
    {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        socket.set_nonblocking(true)?;
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let result = unsafe {
            libc::connect(
                socket.as_raw_fd(),
                &address as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&address) as _,
            )
        };
        if result == 0 {
            return Ok(Stream::Unix(socket));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINPROGRESS) {
            wait_for_connect(&socket, deadline)?;
            return Ok(Stream::Unix(socket));
        }
        if error.raw_os_error() != Some(libc::EAGAIN) && error.kind() != io::ErrorKind::Interrupted
        {
            return Err(error);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Unix connection timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_connect(socket: &UnixStream, deadline: Instant) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Unix connection timed out",
            ));
        }
        let result = unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis() as _) };
        if result > 0 {
            return match socket.take_error()? {
                Some(error) => Err(error),
                None => Ok(()),
            };
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

struct Direction {
    buffer: [u8; 32768],
    start: usize,
    end: usize,
    eof: bool,
}
impl Direction {
    fn new() -> Self {
        Self {
            buffer: [0; 32768],
            start: 0,
            end: 0,
            eof: false,
        }
    }
    fn pump(&mut self, source: &mut Stream, target: &mut Stream) -> io::Result<bool> {
        let mut progress = false;
        if self.start == self.end && !self.eof {
            match source.read(&mut self.buffer) {
                Ok(0) => {
                    self.eof = true;
                    target.shutdown_write();
                    progress = true;
                }
                Ok(n) => {
                    self.start = 0;
                    self.end = n;
                    progress = true;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        if self.start < self.end {
            match target.write(&self.buffer[self.start..self.end]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.start += n;
                    progress = true;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(progress)
    }
}

pub fn relay(mut client: Stream, mut host: Stream, stop: &AtomicBool) -> io::Result<()> {
    client.nonblocking()?;
    host.nonblocking()?;
    let (mut outgoing, mut incoming) = (Direction::new(), Direction::new());
    while !stop.load(Ordering::Relaxed) && !(outgoing.eof && incoming.eof) {
        let progress =
            outgoing.pump(&mut client, &mut host)? | incoming.pump(&mut host, &mut client)?;
        if !progress {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
