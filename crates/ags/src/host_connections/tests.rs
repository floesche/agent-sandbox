use super::*;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::net::UnixStream;
use std::time::Instant;

fn unix_connection(path: &Path) -> Connection {
    Connection {
        name: "test".into(),
        host_unix: Some(path.into()),
        host_tcp: None,
        sandbox_unix: Some(format!("{CONTAINER_DIR}/test.sock")),
        sandbox_tcp: None,
        enabled: true,
        allow_hooks: false,
    }
}
fn read_all(socket: &mut impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).unwrap();
    bytes
}

#[test]
fn unix_relay_preserves_binary_data_half_close_and_permissions() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("upstream");
    let listener = UnixListener::bind(&path).unwrap();
    let bytes = vec![0x8f; 1024 * 1024];
    let expected = bytes.clone();
    let server = thread::spawn(move || {
        drop(listener.accept().unwrap()); // startup probe
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(read_all(&mut stream), expected);
        stream.write_all(b"reply\0after EOF").unwrap();
    });
    let session = Session::start(&[unix_connection(&path)], root.path()).unwrap();
    let dir = session.directory.path().to_owned();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.join("test.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let mut client = UnixStream::connect(dir.join("test.sock")).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client.write_all(&bytes).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(read_all(&mut client), b"reply\0after EOF");
    server.join().unwrap();
    drop(session);
    assert!(!dir.exists());
}

#[test]
fn unix_to_host_tcp_uses_loopback_and_session_drop_closes_active_streams() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = unix_connection(Path::new("/unused"));
    config.host_unix = None;
    config.host_tcp = Some(address);
    let session = Session::start(&[config], root.path()).unwrap();
    drop(listener.accept().unwrap());
    let mut client = UnixStream::connect(session.directory.path().join("test.sock")).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (mut host, _) = listener.accept().unwrap();
    host.write_all(b"ok").unwrap();
    let mut bytes = [0; 2];
    client.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ok");
    let start = Instant::now();
    drop(session);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(read_all(&mut client).is_empty());
}

#[test]
fn upstream_socket_rotation_and_multiple_sessions_are_independent() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("upstream");
    let listener = UnixListener::bind(&path).unwrap();
    let config = unix_connection(&path);
    let a = Session::start(std::slice::from_ref(&config), root.path()).unwrap();
    drop(listener.accept().unwrap());
    let b = Session::start(&[config], root.path()).unwrap();
    drop(listener.accept().unwrap());
    assert_ne!(a.directory.path(), b.directory.path());
    fs::remove_file(&path).unwrap();
    drop(listener);
    let replacement = UnixListener::bind(&path).unwrap();
    drop(a);
    let mut client = UnixStream::connect(b.directory.path().join("test.sock")).unwrap();
    let (mut host, _) = replacement.accept().unwrap();
    host.write_all(b"rotated").unwrap();
    drop(host);
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(read_all(&mut client), b"rotated");
}

#[test]
fn excess_clients_are_closed_without_unbounded_workers() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("upstream");
    let listener = UnixListener::bind(&path).unwrap();
    let session = Session::start(&[unix_connection(&path)], root.path()).unwrap();
    drop(listener.accept().unwrap());
    let socket = session.directory.path().join("test.sock");
    let mut clients = Vec::new();
    let mut hosts = Vec::new();
    for _ in 0..MAX_CLIENTS {
        clients.push(UnixStream::connect(&socket).unwrap());
        hosts.push(listener.accept().unwrap().0);
    }
    let mut excess = UnixStream::connect(&socket).unwrap();
    excess
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert_eq!(excess.read(&mut [0]).unwrap(), 0);
    drop(session);
}

#[test]
fn missing_targets_fail_without_leaving_runtime_directories() {
    let root = tempfile::tempdir().unwrap();
    assert!(Session::start(&[unix_connection(&root.path().join("absent"))], root.path()).is_err());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn full_unix_backlog_has_bounded_connect_timeout() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("full");
    let listener = UnixListener::bind(&path).unwrap();
    use std::os::fd::AsRawFd;
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let _queued = UnixStream::connect(&path).unwrap();
    let start = Instant::now();
    assert!(stream::connect(&unix_connection(&path)).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
}
