//! Connection tests: TLS modes, error classification, server info. Run with `DBSTUDIO_IT=1`.

mod common;

use common::*;
use dbstudio_core::{Error, Session, TlsMode};

async fn tls_state(s: &dyn Session) -> String {
    s.server_info().await.unwrap().extra["tls"].clone()
}

fn cfg_with_ca(t: Target, mode: TlsMode, ca: &str) -> dbstudio_core::ConnectionConfig {
    let mut c = config(t, "dbstudio", mode);
    c.tls.ca_cert_path = Some(certs_dir().join(ca).to_string_lossy().into_owned());
    c
}

#[tokio::test]
async fn tls_modes_on_mysql8() {
    if !it_enabled() {
        return;
    }
    let t = MYSQL8;
    // Disable: no TLS.
    let s = connect_cfg(&config(t, "dbstudio", TlsMode::Disable))
        .await
        .unwrap();
    assert_eq!(tls_state(&*s).await, "off");
    // Prefer and Require: encrypted without verification.
    for mode in [TlsMode::Prefer, TlsMode::Require] {
        let s = connect_cfg(&config(t, "dbstudio", mode)).await.unwrap();
        assert_eq!(tls_state(&*s).await, "on", "{mode:?}");
    }
    // VerifyCa / VerifyFull with the right CA (host 127.0.0.1 is in the certificate SAN).
    for mode in [TlsMode::VerifyCa, TlsMode::VerifyFull] {
        let s = connect_cfg(&cfg_with_ca(t, mode, "ca.crt")).await.unwrap();
        assert_eq!(tls_state(&*s).await, "on", "{mode:?}");
        s.ping().await.unwrap();
    }
    // `localhost` also matches the certificate.
    let mut c = cfg_with_ca(t, TlsMode::VerifyFull, "ca.crt");
    c.host = "localhost".into();
    connect_cfg(&c).await.unwrap();
    // Unrelated CA: chain verification fails.
    for mode in [TlsMode::VerifyCa, TlsMode::VerifyFull] {
        match connect_cfg(&cfg_with_ca(t, mode, "other-ca.crt")).await {
            Err(Error::Tls(_)) => {}
            Err(e) => panic!("{mode:?}: expected Tls error, got {e:?}"),
            Ok(_) => panic!("{mode:?}: unexpectedly connected"),
        }
    }
    // Self-signed chain is not in the built-in root store.
    match connect_cfg(&config(t, "dbstudio", TlsMode::VerifyFull)).await {
        Err(Error::Tls(_)) => {}
        Err(e) => panic!("expected Tls error, got {e:?}"),
        Ok(_) => panic!("unexpectedly connected"),
    }
    // Bad CA file path.
    let mut c = config(t, "dbstudio", TlsMode::VerifyFull);
    c.tls.ca_cert_path = Some("/no/such/ca.pem".into());
    assert!(matches!(connect_cfg(&c).await, Err(Error::Tls(_))));
    // Wrong password over TLS is still an authentication error.
    let mut c = config(t, "dbstudio", TlsMode::VerifyFull);
    c.tls.ca_cert_path = Some(certs_dir().join("ca.crt").to_string_lossy().into_owned());
    let r = MySqlDriverExt::connect_with(&c, "nope").await;
    assert!(
        matches!(r, Err(Error::Authentication(_))),
        "{}",
        describe(&r)
    );
}

#[tokio::test]
async fn tls_modes_on_mariadb() {
    if !it_enabled() {
        return;
    }
    let t = MARIADB;
    let s = connect_cfg(&config(t, "dbstudio", TlsMode::Prefer))
        .await
        .unwrap();
    assert_eq!(
        tls_state(&*s).await,
        "off",
        "Prefer must fall back to plain text"
    );
    for mode in [TlsMode::Require, TlsMode::VerifyCa, TlsMode::VerifyFull] {
        match connect_cfg(&config(t, "dbstudio", mode)).await {
            Err(Error::Tls(_)) => {}
            Err(e) => panic!("{mode:?}: expected Tls error, got {e:?}"),
            Ok(_) => panic!("{mode:?}: connected although the server has no TLS"),
        }
    }
}

struct MySqlDriverExt;
impl MySqlDriverExt {
    async fn connect_with(
        cfg: &dbstudio_core::ConnectionConfig,
        password: &str,
    ) -> dbstudio_core::Result<Box<dyn Session>> {
        use dbstudio_core::Driver;
        dbstudio_driver_mysql::MySqlDriver::new()
            .connect(cfg, Some(&dbstudio_core::Secret::new(password)))
            .await
    }
}

#[tokio::test]
async fn error_classification() {
    if !it_enabled() {
        return;
    }
    for t in TARGETS {
        let cfg = config(t, "dbstudio", TlsMode::Disable);
        // Wrong password.
        let r = MySqlDriverExt::connect_with(&cfg, "wrong-password-xyz").await;
        match r {
            Err(Error::Authentication(m)) => assert!(!m.contains("wrong-password-xyz")),
            other => panic!(
                "[{}] expected Authentication, got {}",
                t.name,
                describe(&other)
            ),
        }
        // Unknown user.
        let mut c = cfg.clone();
        c.username = Some("nobody_here".into());
        let r = MySqlDriverExt::connect_with(&c, "x").await;
        assert!(
            matches!(r, Err(Error::Authentication(_))),
            "[{}] {}",
            t.name,
            describe(&r)
        );
        // Closed port.
        let mut c = cfg.clone();
        c.port = 1;
        let r = MySqlDriverExt::connect_with(&c, "secret-pw-123").await;
        match r {
            Err(Error::Connection(m)) => assert!(!m.contains("secret-pw-123")),
            other => panic!("[{}] expected Connection, got {}", t.name, describe(&other)),
        }
        // Unreachable address with a short timeout.
        let mut c = cfg.clone();
        c.host = "10.255.255.1".into();
        c.connect_timeout_ms = Some(500);
        let started = std::time::Instant::now();
        let r = MySqlDriverExt::connect_with(&c, "x").await;
        assert!(
            matches!(r, Err(Error::Timeout | Error::Connection(_))),
            "[{}] {}",
            t.name,
            describe(&r)
        );
        assert!(started.elapsed().as_secs() < 5);
        // Unknown default database is a connection-time error but not an auth one.
        let mut c = config(t, "root", TlsMode::Disable);
        c.database = Some("no_such_db_zzz".into());
        let r = MySqlDriverExt::connect_with(&c, "dbstudio_test").await;
        assert!(
            matches!(r, Err(Error::Connection(_))),
            "[{}] {}",
            t.name,
            describe(&r)
        );
    }
}

#[tokio::test]
async fn server_info_and_close() {
    if !it_enabled() {
        return;
    }
    for t in TARGETS {
        let s = user_session(t).await;
        s.ping().await.unwrap();
        let info = s.server_info().await.unwrap();
        assert_eq!(
            info.product,
            if t.mariadb { "MariaDB" } else { "MySQL" },
            "[{}]",
            t.name
        );
        assert!(
            info.version
                .starts_with(if t.mariadb { "10.6" } else { "8.4" }),
            "{}",
            info.version
        );
        assert!(!info.extra["charset"].is_empty());
        assert!(info.extra.contains_key("time_zone") && info.extra.contains_key("sql_mode"));
        s.close().await.unwrap();
        assert!(matches!(s.ping().await, Err(Error::Connection(_))));
        // Closing twice is fine.
        s.close().await.unwrap();
    }
}

#[tokio::test]
async fn default_database_and_read_only() {
    if !it_enabled() {
        return;
    }
    for t in TARGETS {
        ensure_fixture(t).await;
        let mut c = config(t, "dbstudio", TlsMode::Disable);
        c.database = Some(FIXTURE_DB.into());
        c.read_only = true;
        let s = connect_cfg(&c).await.unwrap();
        let rs = query(&*s, "SELECT DATABASE()").await;
        assert_eq!(rs.rows[0][0], dbstudio_core::Value::Text(FIXTURE_DB.into()));
        let r = dbstudio_core::execute_collect(
            &*s,
            dbstudio_core::QueryRequest::new("DELETE FROM people"),
        )
        .await;
        assert!(
            matches!(r, Err(Error::Query { .. })),
            "[{}] read-only config must refuse writes: {:?}",
            t.name,
            r
        );
        assert_eq!(
            query(&*s, "SELECT COUNT(*) FROM people").await.rows[0][0],
            dbstudio_core::Value::Int(7)
        );
    }
}
