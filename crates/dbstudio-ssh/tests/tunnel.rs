//! Tunnel tests against an in-process SSH server (no Docker): password and key authentication,
//! host key policy, forwarding, failure modes.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dbstudio_core::{Error, Secret, SshAuth, SshConfig};
use dbstudio_ssh::{HostKeyStatus, KnownHosts, MemoryKnownHosts, Tunnel};
use russh::keys::{Algorithm, PrivateKey, ssh_key::LineEnding};
use russh::server::{Auth, Handler, Msg, Server as _, Session, VerifiedPublicKey};
use russh::{Channel, MethodKind, MethodSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const USER: &str = "dbstudio";
const PASSWORD: &str = "dbstudio_ssh";
const PASSPHRASE: &str = "key pass";

struct TestServer {
    addr: SocketAddr,
    fingerprint: String,
    client_key_plain: PathBuf,
    client_key_encrypted: PathBuf,
    _dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct Srv {
    authorized: Arc<russh::keys::PublicKey>,
}

impl russh::server::Server for Srv {
    type Handler = Self;
    fn new_client(&mut self, _: Option<SocketAddr>) -> Self {
        self.clone()
    }
}

impl Handler for Srv {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == USER && password == PASSWORD {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        if user == USER && key.public_key.key_data() == self.authorized.key_data() {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _orig: &str,
        _orig_port: u32,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        match TcpStream::connect((host, port as u16)).await {
            Ok(mut upstream) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                });
            }
            Err(_) => reply.reject(russh::ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}

async fn start_server() -> TestServer {
    let mut rng = rand::rng();
    let host_key = PrivateKey::random(&mut rng, Algorithm::Ed25519).unwrap();
    let fingerprint = host_key
        .public_key()
        .fingerprint(russh::keys::HashAlg::Sha256)
        .to_string();
    let client_key = PrivateKey::random(&mut rng, Algorithm::Ed25519).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("id_plain");
    std::fs::write(
        &plain,
        client_key.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    let encrypted = dir.path().join("id_enc");
    let enc = client_key.encrypt(&mut rng, PASSPHRASE).unwrap();
    std::fs::write(
        &encrypted,
        enc.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();

    let config = russh::server::Config {
        keys: vec![host_key],
        methods: MethodSet::from(&[MethodKind::Password, MethodKind::PublicKey][..]),
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        ..Default::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut srv = Srv {
        authorized: Arc::new(client_key.public_key().clone()),
    };
    let task = tokio::spawn(async move {
        let _ = srv.run_on_socket(Arc::new(config), &listener).await;
    });
    TestServer {
        addr,
        fingerprint,
        client_key_plain: plain,
        client_key_encrypted: encrypted,
        _dir: dir,
        task,
    }
}

async fn echo_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let t = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (addr, t)
}

fn cfg(addr: SocketAddr, auth: SshAuth) -> SshConfig {
    SshConfig {
        host: "127.0.0.1".into(),
        port: addr.port(),
        username: USER.into(),
        auth,
        keepalive_secs: Some(30),
        connect_timeout_ms: Some(10_000),
    }
}

fn trusted(s: &TestServer) -> MemoryKnownHosts {
    let k = MemoryKnownHosts::new();
    k.trust("127.0.0.1", s.addr.port(), "ssh-ed25519", &s.fingerprint);
    k
}

async fn roundtrip(t: &Tunnel, msg: &[u8]) -> Vec<u8> {
    let mut c = TcpStream::connect(t.local_addr()).await.unwrap();
    c.write_all(msg).await.unwrap();
    let mut got = vec![0u8; msg.len()];
    tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut got))
        .await
        .expect("echo timed out")
        .unwrap();
    got
}

#[tokio::test]
async fn password_auth_forwards_traffic() {
    let s = start_server().await;
    let (echo, _e) = echo_server().await;
    let t = Tunnel::open(
        &cfg(s.addr, SshAuth::Password),
        Some(&Secret::new(PASSWORD)),
        ("127.0.0.1".into(), echo.port()),
        &trusted(&s),
    )
    .await
    .unwrap();
    assert!(t.local_addr().ip().is_loopback());
    assert_eq!(roundtrip(&t, b"hello tunnel").await, b"hello tunnel");
    // several concurrent connections through the same tunnel
    let (a, b) = tokio::join!(roundtrip(&t, b"one"), roundtrip(&t, b"two two"));
    assert_eq!(a, b"one");
    assert_eq!(b, b"two two");
}

#[tokio::test]
async fn private_key_with_and_without_passphrase() {
    let s = start_server().await;
    let (echo, _e) = echo_server().await;
    let plain = SshAuth::PrivateKey {
        key_path: s.client_key_plain.display().to_string(),
        has_passphrase: false,
    };
    let t = Tunnel::open(
        &cfg(s.addr, plain),
        None,
        ("127.0.0.1".into(), echo.port()),
        &trusted(&s),
    )
    .await
    .unwrap();
    assert_eq!(roundtrip(&t, b"k1").await, b"k1");

    let enc = SshAuth::PrivateKey {
        key_path: s.client_key_encrypted.display().to_string(),
        has_passphrase: true,
    };
    let t = Tunnel::open(
        &cfg(s.addr, enc.clone()),
        Some(&Secret::new(PASSPHRASE)),
        ("127.0.0.1".into(), echo.port()),
        &trusted(&s),
    )
    .await
    .unwrap();
    assert_eq!(roundtrip(&t, b"k2").await, b"k2");

    // wrong passphrase, missing passphrase
    let e = Tunnel::open(
        &cfg(s.addr, enc.clone()),
        Some(&Secret::new("nope")),
        ("127.0.0.1".into(), echo.port()),
        &trusted(&s),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, Error::Authentication(_)), "{e:?}");
    let e = Tunnel::open(
        &cfg(s.addr, enc),
        None,
        ("127.0.0.1".into(), echo.port()),
        &trusted(&s),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, Error::Authentication(_)), "{e:?}");
}

#[tokio::test]
async fn missing_key_file_is_an_authentication_error() {
    let s = start_server().await;
    let auth = SshAuth::PrivateKey {
        key_path: "/definitely/not/here".into(),
        has_passphrase: false,
    };
    let e = Tunnel::open(
        &cfg(s.addr, auth),
        None,
        ("127.0.0.1".into(), 1),
        &trusted(&s),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(e, Error::Authentication(m) if m.contains("cannot read")),
        ""
    );
}

#[tokio::test]
async fn unknown_host_key_is_reported_before_authentication() {
    let s = start_server().await;
    let e = Tunnel::open(
        &cfg(s.addr, SshAuth::Password),
        Some(&Secret::new(PASSWORD)),
        ("127.0.0.1".into(), 1),
        &MemoryKnownHosts::new(),
    )
    .await
    .unwrap_err();
    match e {
        Error::SshHostKey {
            host,
            port,
            key_type,
            fingerprint,
            changed,
        } => {
            assert_eq!(host, "127.0.0.1");
            assert_eq!(port, s.addr.port());
            assert_eq!(key_type, "ssh-ed25519");
            assert_eq!(fingerprint, s.fingerprint);
            assert!(fingerprint.starts_with("SHA256:"));
            assert!(!changed);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn changed_host_key_is_flagged_and_trusting_it_unblocks() {
    let s = start_server().await;
    let known = MemoryKnownHosts::new();
    known.trust(
        "127.0.0.1",
        s.addr.port(),
        "ssh-ed25519",
        "SHA256:someoneelse",
    );
    let c = cfg(s.addr, SshAuth::Password);
    let e = Tunnel::open(
        &c,
        Some(&Secret::new(PASSWORD)),
        ("127.0.0.1".into(), 1),
        &known,
    )
    .await
    .unwrap_err();
    let Error::SshHostKey {
        fingerprint,
        key_type,
        changed,
        ..
    } = e
    else {
        panic!("{e:?}")
    };
    assert!(changed);
    // The UI shows the fingerprint, the user trusts it, the connection is opened again.
    known.trust("127.0.0.1", s.addr.port(), &key_type, &fingerprint);
    assert_eq!(
        known.check("127.0.0.1", s.addr.port(), &key_type, &fingerprint),
        HostKeyStatus::Known
    );
    let (echo, _e) = echo_server().await;
    let t = Tunnel::open(
        &c,
        Some(&Secret::new(PASSWORD)),
        ("127.0.0.1".into(), echo.port()),
        &known,
    )
    .await
    .unwrap();
    assert_eq!(roundtrip(&t, b"ok").await, b"ok");
}

#[tokio::test]
async fn wrong_or_missing_password_is_an_authentication_error() {
    let s = start_server().await;
    let c = cfg(s.addr, SshAuth::Password);
    let e = Tunnel::open(
        &c,
        Some(&Secret::new("bad")),
        ("127.0.0.1".into(), 1),
        &trusted(&s),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, Error::Authentication(_)), "{e:?}");
    let e = Tunnel::open(&c, None, ("127.0.0.1".into(), 1), &trusted(&s))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Authentication(_)), "{e:?}");
    // The error text never contains the password.
    let e = Tunnel::open(
        &c,
        Some(&Secret::new("sup3rsecret")),
        ("127.0.0.1".into(), 1),
        &trusted(&s),
    )
    .await
    .unwrap_err();
    assert!(!e.to_string().contains("sup3rsecret"));
}

#[tokio::test]
async fn unreachable_bastion_is_a_connection_error() {
    // A port nobody listens on.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let e = Tunnel::open(
        &cfg(addr, SshAuth::Password),
        Some(&Secret::new("x")),
        ("127.0.0.1".into(), 1),
        &MemoryKnownHosts::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, Error::Connection(_)), "{e:?}");
}

#[tokio::test]
async fn silent_bastion_times_out() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let _hold = tokio::spawn(async move {
        let mut socks = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            socks.push(s); // accept and say nothing
        }
    });
    let mut c = cfg(addr, SshAuth::Password);
    c.connect_timeout_ms = Some(400);
    let e = Tunnel::open(
        &c,
        Some(&Secret::new("x")),
        ("127.0.0.1".into(), 1),
        &MemoryKnownHosts::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, Error::Timeout), "{e:?}");
}

#[tokio::test]
async fn dropping_the_tunnel_closes_the_listener() {
    let s = start_server().await;
    let (echo, _e) = echo_server().await;
    let t = Tunnel::open(
        &cfg(s.addr, SshAuth::Password),
        Some(&Secret::new(PASSWORD)),
        ("127.0.0.1".into(), echo.port()),
        &trusted(&s),
    )
    .await
    .unwrap();
    let local = t.local_addr();
    assert_eq!(roundtrip(&t, b"x").await, b"x");
    drop(t);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(TcpStream::connect(local).await.is_err());
}

#[tokio::test]
async fn closed_target_port_closes_that_connection_only() {
    let s = start_server().await;
    let (echo, _e) = echo_server().await;
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let t_dead = Tunnel::open(
        &cfg(s.addr, SshAuth::Password),
        Some(&Secret::new(PASSWORD)),
        ("127.0.0.1".into(), dead),
        &trusted(&s),
    )
    .await
    .unwrap();
    let mut c = TcpStream::connect(t_dead.local_addr()).await.unwrap();
    let _ = c.write_all(b"x").await;
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("connection should be closed")
        .unwrap_or(0);
    assert_eq!(n, 0);
    // The tunnel itself keeps working for other targets.
    let t_ok = Tunnel::open(
        &cfg(s.addr, SshAuth::Password),
        Some(&Secret::new(PASSWORD)),
        ("127.0.0.1".into(), echo.port()),
        &trusted(&s),
    )
    .await
    .unwrap();
    assert_eq!(roundtrip(&t_ok, b"fine").await, b"fine");
}
