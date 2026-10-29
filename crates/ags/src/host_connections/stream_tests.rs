use super::*;
use std::os::unix::net::UnixListener;

#[test]
fn unix_connections_are_nonblocking_and_close_on_exec() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("upstream");
    let _listener = UnixListener::bind(&path).unwrap();
    let connection = Connection {
        name: "flags".into(),
        host_unix: Some(path),
        host_tcp: None,
        sandbox_unix: Some("/run/ags-connections/flags.sock".into()),
        sandbox_tcp: None,
        enabled: true,
        allow_hooks: false,
    };
    let Stream::Unix(socket) = connect(&connection).unwrap() else {
        panic!("expected Unix connection");
    };
    let fd = socket.as_raw_fd();
    assert_ne!(
        unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    assert_ne!(
        unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
}

#[test]
fn pending_connect_wait_checks_readiness_and_deadline() {
    let (socket, _peer) = UnixStream::pair().unwrap();
    wait_for_connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
    assert_eq!(
        wait_for_connect(&socket, Instant::now())
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut,
    );
}
