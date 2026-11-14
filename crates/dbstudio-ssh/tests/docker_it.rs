//! Integration tests against the `sshd` compose service (run with `DBSTUDIO_IT=1`):
//! tunnels to pg16 / mysql8 / mssql2022 by compose service name, with a password and with the
//! passphrase protected key `docker/ssh/id_ed25519` (passphrase `dbstudio_key_pass`).

use std::time::Duration;

use dbstudio_core::{Error, Secret, SshAuth, SshConfig};
use dbstudio_ssh::{MemoryKnownHosts, Tunnel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").is_ok_and(|v| v == "1")
}

fn ssh_port() -> u16 {
    std::env::var("DBSTUDIO_IT_SSH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(22022)
}

fn key_path() -> String {
    format!("{}/../../docker/ssh/id_ed25519", env!("CARGO_MANIFEST_DIR"))
}

fn cfg(auth: SshAuth) -> SshConfig {
    SshConfig {
        host: "127.0.0.1".into(),
        port: ssh_port(),
        username: "dbstudio".into(),
        auth,
        keepalive_secs: Some(15),
        connect_timeout_ms: Some(15_000),
    }
}

fn password() -> (SshConfig, Secret) {
    (cfg(SshAuth::Password), Secret::new("dbstudio_ssh"))
}

fn key() -> (SshConfig, Secret) {
    (
        cfg(SshAuth::PrivateKey {
            key_path: key_path(),
            has_passphrase: true,
        }),
        Secret::new("dbstudio_key_pass"),
    )
}

/// First connection fails with `SshHostKey`; "trusting" the reported key makes the second work.
async fn open(cfgs: (SshConfig, Secret), target: (&str, u16)) -> Tunnel {
    let (cfg, secret) = cfgs;
    let known = MemoryKnownHosts::new();
    let target = (target.0.to_string(), target.1);
    let err = Tunnel::open(&cfg, Some(&secret), target.clone(), &known)
        .await
        .unwrap_err();
    let Error::SshHostKey {
        host,
        port,
        key_type,
        fingerprint,
        changed,
    } = err
    else {
        panic!("expected sshHostKey, got {err:?}")
    };
    assert!(!changed);
    assert_eq!((host.as_str(), port), ("127.0.0.1", cfg.port));
    assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
    known.trust(&host, port, &key_type, &fingerprint);
    Tunnel::open(&cfg, Some(&secret), target, &known)
        .await
        .expect("tunnel after trusting the host key")
}

async fn read_with_timeout(c: &mut TcpStream, buf: &mut [u8]) -> usize {
    tokio::time::timeout(Duration::from_secs(10), c.read(buf))
        .await
        .expect("server answer timed out")
        .expect("read")
}

/// PostgreSQL answers an SSLRequest with a single `S` or `N`.
async fn assert_postgres(t: &Tunnel) {
    let mut c = TcpStream::connect(t.local_addr()).await.unwrap();
    c.write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .await
        .unwrap();
    let mut b = [0u8; 1];
    assert_eq!(read_with_timeout(&mut c, &mut b).await, 1);
    assert!(b[0] == b'S' || b[0] == b'N', "unexpected byte {:#x}", b[0]);
}

/// MySQL speaks first: 3 byte length, sequence 0, protocol version 10.
async fn assert_mysql(t: &Tunnel) {
    let mut c = TcpStream::connect(t.local_addr()).await.unwrap();
    let mut b = [0u8; 8];
    let n = read_with_timeout(&mut c, &mut b).await;
    assert!(n >= 5, "short greeting");
    assert_eq!(b[3], 0, "sequence id");
    assert_eq!(b[4], 10, "protocol version");
}

/// TDS pre-login response has packet type 4.
async fn assert_mssql(t: &Tunnel) {
    let mut c = TcpStream::connect(t.local_addr()).await.unwrap();
    // Minimal PRELOGIN: VERSION option + terminator.
    let payload: [u8; 12] = [
        0x00, 0x00, 0x06, 0x00, 0x06, 0xff, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mut packet = vec![0x12, 0x01, 0x00, (8 + payload.len()) as u8, 0, 0, 1, 0];
    packet.extend_from_slice(&payload);
    c.write_all(&packet).await.unwrap();
    let mut b = [0u8; 8];
    let n = read_with_timeout(&mut c, &mut b).await;
    assert!(n >= 8);
    assert_eq!(b[0], 4, "TDS response packet type");
}

#[tokio::test]
async fn password_tunnel_reaches_postgres_and_mysql() {
    if !enabled() {
        return;
    }
    let pg = open(password(), ("pg16", 5432)).await;
    assert_postgres(&pg).await;
    let my = open(password(), ("mysql8", 3306)).await;
    assert_mysql(&my).await;
}

#[tokio::test]
async fn key_with_passphrase_tunnel_reaches_postgres_and_mysql() {
    if !enabled() {
        return;
    }
    let pg = open(key(), ("pg16", 5432)).await;
    assert_postgres(&pg).await;
    let my = open(key(), ("mysql8", 3306)).await;
    assert_mysql(&my).await;
}

#[tokio::test]
async fn tunnel_reaches_mssql_when_it_runs() {
    if !enabled() {
        return;
    }
    // MSSQL is not always up (2 GB): probe the published port first and skip when it is closed,
    // unless `DBSTUDIO_IT_STRICT` names `mssql2022` (or is `1`, CI), where a missing container is a failure.
    let port: u16 = std::env::var("DBSTUDIO_IT_MSSQL_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(14333);
    let probe = tokio::time::timeout(
        Duration::from_secs(1),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await;
    if !matches!(probe, Ok(Ok(_))) {
        if dbstudio_core::testing::it_strict_for("mssql2022") {
            panic!("mssql2022 is not reachable on 127.0.0.1:{port} (DBSTUDIO_IT_STRICT=1)");
        }
        eprintln!("skip: mssql2022 is not running on 127.0.0.1:{port}");
        return;
    }
    let t = open(password(), ("mssql2022", 1433)).await;
    assert_mssql(&t).await;
}

#[tokio::test]
async fn several_connections_share_one_tunnel() {
    if !enabled() {
        return;
    }
    let t = open(password(), ("pg16", 5432)).await;
    let (a, b, c) = tokio::join!(
        assert_postgres(&t),
        assert_postgres(&t),
        assert_postgres(&t)
    );
    let _ = (a, b, c);
}

#[tokio::test]
async fn wrong_credentials_are_rejected_and_host_key_is_checked_first() {
    if !enabled() {
        return;
    }
    // With an empty known-hosts table the host key error wins over the credentials.
    let (cfg, _) = password();
    let e = Tunnel::open(
        &cfg,
        Some(&Secret::new("wrong")),
        ("pg16".into(), 5432),
        &MemoryKnownHosts::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, Error::SshHostKey { .. }), "{e:?}");
    // After trusting it, the wrong password is an authentication error.
    let known = MemoryKnownHosts::new();
    if let Error::SshHostKey {
        key_type,
        fingerprint,
        ..
    } = e
    {
        known.trust("127.0.0.1", cfg.port, &key_type, &fingerprint);
    }
    let e = Tunnel::open(
        &cfg,
        Some(&Secret::new("wrong")),
        ("pg16".into(), 5432),
        &known,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, Error::Authentication(_)), "{e:?}");
    // Wrong key passphrase.
    let (kcfg, _) = key();
    let e = Tunnel::open(
        &kcfg,
        Some(&Secret::new("wrong")),
        ("pg16".into(), 5432),
        &known,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, Error::Authentication(_)), "{e:?}");
}
