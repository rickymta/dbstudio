//! Connection URI parsing (package K, CONN-13).
//!
//! `connection_parse_uri` turns `mongodb://`, `mongodb+srv://`, `redis://`, `rediss://`,
//! `postgres(ql)://`, `mysql://`, `sqlserver://`, `oracle://`, `cockroachdb://`, `yugabytedb://`,
//! `tidb://`, `clickhouse[s]://`, `cassandra://` and `scylladb://` URIs into a connection profile.
//! The password never goes to the frontend and is not kept in memory either: the command
//! reports `hasPassword` and the UI asks the user to type the password again.

use dbstudio_core::{ConnectionConfig, DbKind, Error, TlsMode};
use dbstudio_store::ConnectionProfile;
use serde::Serialize;
use tauri::State;

use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

/// Result of `connection_parse_uri`. The password is dropped: `has_password` tells the UI to ask
/// for it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedConnectionUri {
    pub profile: ConnectionProfile,
    pub has_password: bool,
}

fn invalid(msg: &str) -> Error {
    // Never echo the URI: it may contain the password.
    Error::Invalid(format!("invalid connection URI: {msg}"))
}

fn pct_decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s
                    .get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| invalid("bad percent escape"))?;
                out.push(hex);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| invalid("not valid UTF-8"))
}

/// Same as [`pct_decode`] but `+` stays (user info and paths).
fn pct_decode_strict(s: &str) -> Result<String> {
    pct_decode(&s.replace('+', "%2B"))
}

struct Parts {
    scheme: String,
    user: Option<String>,
    password: Option<String>,
    hosts: Vec<(String, Option<u16>)>,
    path: Option<String>,
    query: Vec<(String, String)>,
}

fn split_host_port(h: &str) -> Result<(String, Option<u16>)> {
    if let Some(rest) = h.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| invalid("bad IPv6 host"))?;
        let port = match tail.strip_prefix(':') {
            Some(p) if !p.is_empty() => Some(p.parse().map_err(|_| invalid("bad port"))?),
            _ => None,
        };
        return Ok((host.to_string(), port));
    }
    match h.rsplit_once(':') {
        Some((host, p)) if !p.is_empty() => Ok((
            pct_decode_strict(host)?,
            Some(p.parse().map_err(|_| invalid("bad port"))?),
        )),
        Some((host, _)) => Ok((pct_decode_strict(host)?, None)),
        None => Ok((pct_decode_strict(h)?, None)),
    }
}

fn split_uri(uri: &str) -> Result<Parts> {
    let uri = uri.trim();
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or_else(|| invalid("expected scheme://"))?;
    let scheme = scheme.to_ascii_lowercase();
    // SQL Server also uses `;key=value` parameters.
    let (rest, semi) = match (scheme.as_str(), rest.split_once(';')) {
        ("sqlserver", Some((r, p))) => (r, Some(p)),
        _ => (rest, None),
    };
    let (rest, query_text) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (rest, None),
    };
    // The authority ends at the first '/' after the LAST '@', so an unencoded '/' inside the
    // password cannot leak into the host or database.
    let search_from = rest.rfind('@').map(|i| i + 1).unwrap_or(0);
    let (authority, path) = match rest[search_from..].find('/') {
        Some(i) => (&rest[..search_from + i], Some(&rest[search_from + i + 1..])),
        None => (rest, None),
    };
    let (userinfo, hosts_text) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((n, p)) => (Some(pct_decode_strict(n)?), Some(pct_decode_strict(p)?)),
            None => (Some(pct_decode_strict(u)?), None),
        },
        None => (None, None),
    };
    let hosts = hosts_text
        .split(',')
        .filter(|h| !h.is_empty())
        .map(split_host_port)
        .collect::<Result<Vec<_>>>()?;
    if hosts.is_empty() {
        return Err(invalid("missing host"));
    }
    let mut query = Vec::new();
    for text in [query_text, semi].into_iter().flatten() {
        for pair in text.split(['&', ';']).filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            query.push((pct_decode(k)?.to_ascii_lowercase(), pct_decode(v)?));
        }
    }
    let path = match path {
        Some(p) if !p.is_empty() => Some(pct_decode_strict(p)?),
        _ => None,
    };
    Ok(Parts {
        scheme,
        user: user.filter(|u| !u.is_empty()),
        password: password.filter(|p| !p.is_empty()),
        hosts,
        path,
        query,
    })
}

fn q<'a>(parts: &'a Parts, key: &str) -> Option<&'a str> {
    parts
        .query
        .iter()
        .find(|(k, _)| k == &key.to_ascii_lowercase())
        .map(|(_, v)| v.as_str())
}

fn truthy(v: Option<&str>) -> Option<bool> {
    v.map(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "required"
        )
    })
}

fn profile_of(config: ConnectionConfig) -> ConnectionProfile {
    ConnectionProfile {
        config,
        group_id: None,
        color: None,
        save_password: false,
        has_password: false,
        sort_order: 0,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn host_label(parts: &Parts, db: Option<&str>) -> String {
    let h = &parts.hosts[0].0;
    match db {
        Some(d) => format!("{h}/{d}"),
        None => h.clone(),
    }
}

fn parse_mongo(p: &Parts) -> Result<ConnectionConfig> {
    let srv = p.scheme == "mongodb+srv";
    if srv && p.hosts.len() != 1 {
        return Err(invalid("mongodb+srv takes exactly one host"));
    }
    let db = p.path.clone();
    let mut cfg = ConnectionConfig::new(
        DbKind::MongoDb,
        host_label(p, db.as_deref()),
        p.hosts[0].0.clone(),
    );
    cfg.port = p.hosts[0].1.unwrap_or(27017);
    cfg.database = db;
    cfg.username = p.user.clone();
    if srv {
        cfg.options.insert("srv".into(), "true".into());
    }
    if p.hosts.len() > 1 {
        let all: Vec<String> = p
            .hosts
            .iter()
            .map(|(h, port)| {
                let h = if h.contains(':') {
                    format!("[{h}]")
                } else {
                    h.clone()
                };
                format!("{h}:{}", port.unwrap_or(27017))
            })
            .collect();
        cfg.options.insert("hosts".into(), all.join(","));
    }
    for (uri_key, opt_key) in [
        ("authsource", "authSource"),
        ("authmechanism", "authMechanism"),
        ("replicaset", "replicaSet"),
        ("directconnection", "directConnection"),
        ("appname", "appName"),
    ] {
        if let Some(v) = q(p, uri_key).filter(|v| !v.is_empty()) {
            cfg.options.insert(opt_key.into(), v.to_string());
        }
    }
    let tls = truthy(q(p, "tls").or_else(|| q(p, "ssl"))).unwrap_or(srv);
    let insecure = truthy(q(p, "tlsInsecure")).unwrap_or(false)
        || truthy(q(p, "tlsAllowInvalidCertificates")).unwrap_or(false);
    cfg.tls.mode = match (tls, insecure) {
        (false, _) if q(p, "tls").is_some() || q(p, "ssl").is_some() => TlsMode::Disable,
        (false, _) => TlsMode::Prefer,
        (true, true) => TlsMode::Require,
        (true, false) if truthy(q(p, "tlsAllowInvalidHostnames")).unwrap_or(false) => {
            TlsMode::VerifyCa
        }
        (true, false) => TlsMode::VerifyFull,
    };
    cfg.tls.ca_cert_path = q(p, "tlsCAFile").map(str::to_string);
    cfg.tls.client_cert_path = q(p, "tlsCertificateKeyFile").map(str::to_string);
    if let Some(ms) = q(p, "connectTimeoutMS").and_then(|v| v.parse().ok()) {
        cfg.connect_timeout_ms = Some(ms);
    }
    Ok(cfg)
}

fn parse_redis(p: &Parts) -> Result<ConnectionConfig> {
    let db = p.path.as_deref().filter(|d| !d.is_empty());
    if let Some(d) = db {
        if d.parse::<u32>().is_err() {
            return Err(invalid("the Redis database must be a number"));
        }
    }
    let mut cfg = ConnectionConfig::new(DbKind::Redis, host_label(p, db), p.hosts[0].0.clone());
    cfg.port = p.hosts[0].1.unwrap_or(6379);
    cfg.database = db.map(str::to_string);
    cfg.username = p.user.clone();
    cfg.tls.mode = if p.scheme == "rediss" {
        TlsMode::VerifyFull
    } else {
        TlsMode::Prefer
    };
    Ok(cfg)
}

fn tls_from_sslmode(mode: &str) -> Option<TlsMode> {
    Some(match mode.to_ascii_lowercase().replace('_', "-").as_str() {
        "disable" | "disabled" | "false" => TlsMode::Disable,
        "allow" | "prefer" | "preferred" => TlsMode::Prefer,
        "require" | "required" | "true" => TlsMode::Require,
        "verify-ca" => TlsMode::VerifyCa,
        "verify-full" | "verify-identity" => TlsMode::VerifyFull,
        _ => return None,
    })
}

fn parse_sql(p: &Parts) -> Result<ConnectionConfig> {
    let (kind, default_port) = match p.scheme.as_str() {
        "postgres" | "postgresql" => (DbKind::Postgres, 5432),
        "mysql" | "mariadb" => (DbKind::MySql, 3306),
        "sqlserver" | "mssql" => (DbKind::SqlServer, 1433),
        "oracle" => (DbKind::Oracle, 1521),
        "cockroachdb" | "cockroach" => (DbKind::CockroachDb, 26257),
        "yugabytedb" | "yugabyte" => (DbKind::YugabyteDb, 5433),
        "tidb" => (DbKind::TiDb, 4000),
        _ => unreachable!("scheme checked by the caller"),
    };
    let db = p
        .path
        .clone()
        .or_else(|| q(p, "database").map(str::to_string))
        .or_else(|| q(p, "databasename").map(str::to_string))
        .filter(|d| !d.is_empty());
    let mut cfg = ConnectionConfig::new(kind, host_label(p, db.as_deref()), p.hosts[0].0.clone());
    cfg.port = p.hosts[0].1.unwrap_or(default_port);
    cfg.database = db;
    cfg.username = p.user.clone().or_else(|| q(p, "user").map(str::to_string));
    if let Some(m) = q(p, "sslmode")
        .or_else(|| q(p, "ssl-mode"))
        .and_then(tls_from_sslmode)
    {
        cfg.tls.mode = m;
    } else if let Some(enc) = truthy(q(p, "encrypt")) {
        cfg.tls.mode = if !enc {
            TlsMode::Disable
        } else if truthy(q(p, "trustServerCertificate")).unwrap_or(false) {
            TlsMode::Require
        } else {
            TlsMode::VerifyFull
        };
    } else if let Some(t) = truthy(q(p, "ssl")).or_else(|| truthy(q(p, "tls"))) {
        cfg.tls.mode = if t {
            TlsMode::Require
        } else {
            TlsMode::Disable
        };
    }
    cfg.tls.ca_cert_path = q(p, "sslrootcert")
        .or_else(|| q(p, "ssl-ca"))
        .map(str::to_string);
    cfg.tls.client_cert_path = q(p, "sslcert")
        .or_else(|| q(p, "ssl-cert"))
        .map(str::to_string);
    cfg.tls.client_key_path = q(p, "sslkey")
        .or_else(|| q(p, "ssl-key"))
        .map(str::to_string);
    if let Some(secs) = q(p, "connect_timeout").and_then(|v| v.parse::<u64>().ok()) {
        cfg.connect_timeout_ms = Some(secs * 1000);
    }
    // CockroachDB Cloud routes by cluster id (`options=--cluster=<id>` at startup).
    if kind == DbKind::CockroachDb {
        let cluster = q(p, "cluster").map(str::to_string).or_else(|| {
            q(p, "options").and_then(|o| {
                o.split_whitespace()
                    .find_map(|w| w.strip_prefix("--cluster=").map(str::to_string))
            })
        });
        if let Some(c) = cluster.filter(|c| !c.is_empty()) {
            cfg.options.insert("cockroach.cluster".into(), c);
        }
    }
    Ok(cfg)
}

/// `clickhouse://` (HTTP, 8123) and `clickhouses://` (HTTPS, 8443); `?secure=true` also
/// turns TLS on, `?compress=` and `?editing=` are kept for the driver.
fn parse_clickhouse(p: &Parts) -> Result<ConnectionConfig> {
    let secure = p.scheme == "clickhouses"
        || truthy(q(p, "secure"))
            .or_else(|| truthy(q(p, "ssl")))
            .or_else(|| truthy(q(p, "tls")))
            .unwrap_or(false);
    let db = p
        .path
        .clone()
        .or_else(|| q(p, "database").map(str::to_string))
        .filter(|d| !d.is_empty());
    let mut cfg = ConnectionConfig::new(
        DbKind::ClickHouse,
        host_label(p, db.as_deref()),
        p.hosts[0].0.clone(),
    );
    cfg.port = p.hosts[0].1.unwrap_or(if secure { 8443 } else { 8123 });
    cfg.database = db;
    cfg.username = p.user.clone().or_else(|| q(p, "user").map(str::to_string));
    cfg.tls.mode = match (secure, q(p, "sslmode").and_then(tls_from_sslmode)) {
        (_, Some(m)) => m,
        (true, None) if truthy(q(p, "skip_verify")).unwrap_or(false) => TlsMode::Require,
        (true, None) => TlsMode::VerifyFull,
        (false, None) => TlsMode::Disable,
    };
    cfg.tls.ca_cert_path = q(p, "sslrootcert").map(str::to_string);
    for (uri_key, opt_key) in [("compress", "compress"), ("editing", "editing")] {
        if let Some(v) = q(p, uri_key).filter(|v| !v.is_empty()) {
            cfg.options.insert(opt_key.into(), v.to_string());
        }
    }
    if let Some(ms) = q(p, "connect_timeout").and_then(|v| v.parse::<u64>().ok()) {
        cfg.connect_timeout_ms = Some(ms * 1000);
    }
    Ok(cfg)
}

/// `cassandra://` and `scylladb://` (native protocol, 9042); the path is the keyspace.
fn parse_cassandra(p: &Parts) -> Result<ConnectionConfig> {
    let kind = if p.scheme == "cassandra" {
        DbKind::Cassandra
    } else {
        DbKind::ScyllaDb
    };
    let db = p
        .path
        .clone()
        .or_else(|| q(p, "keyspace").map(str::to_string))
        .filter(|d| !d.is_empty());
    let mut cfg = ConnectionConfig::new(kind, host_label(p, db.as_deref()), p.hosts[0].0.clone());
    cfg.port = p.hosts[0].1.unwrap_or(9042);
    cfg.database = db;
    cfg.username = p.user.clone().or_else(|| q(p, "user").map(str::to_string));
    if p.hosts.len() > 1 {
        let all: Vec<String> = p
            .hosts
            .iter()
            .map(|(h, port)| {
                let h = if h.contains(':') {
                    format!("[{h}]")
                } else {
                    h.clone()
                };
                format!("{h}:{}", port.unwrap_or(9042))
            })
            .collect();
        cfg.options.insert("hosts".into(), all.join(","));
    }
    for (uri_key, opt_key) in [
        ("localdc", "localDc"),
        ("local_dc", "localDc"),
        ("consistency", "consistency"),
        ("lwtediting", "lwtEdits"),
        ("lwtedits", "lwtEdits"),
    ] {
        if let Some(v) = q(p, uri_key).filter(|v| !v.is_empty()) {
            cfg.options.insert(opt_key.into(), v.to_string());
        }
    }
    cfg.tls.mode = match (
        q(p, "sslmode").and_then(tls_from_sslmode),
        truthy(q(p, "ssl")).or_else(|| truthy(q(p, "tls"))),
    ) {
        (Some(m), _) => m,
        (None, Some(true)) => TlsMode::VerifyFull,
        (None, Some(false)) => TlsMode::Disable,
        (None, None) => TlsMode::Prefer,
    };
    cfg.tls.ca_cert_path = q(p, "sslrootcert").map(str::to_string);
    if let Some(ms) = q(p, "connect_timeout").and_then(|v| v.parse::<u64>().ok()) {
        cfg.connect_timeout_ms = Some(ms * 1000);
    }
    Ok(cfg)
}

/// Parse a URI into a profile and whether it carried a password (which is discarded).
pub fn parse_uri(uri: &str) -> Result<(ConnectionProfile, bool)> {
    let parts = split_uri(uri)?;
    let cfg = match parts.scheme.as_str() {
        "mongodb" | "mongodb+srv" => parse_mongo(&parts)?,
        "redis" | "rediss" => parse_redis(&parts)?,
        "postgres" | "postgresql" | "mysql" | "mariadb" | "sqlserver" | "mssql" | "oracle"
        | "cockroachdb" | "cockroach" | "yugabytedb" | "yugabyte" | "tidb" => parse_sql(&parts)?,
        "clickhouse" | "clickhouses" => parse_clickhouse(&parts)?,
        "cassandra" | "scylladb" | "scylla" => parse_cassandra(&parts)?,
        other => return Err(Error::Unsupported(format!("uri_scheme:{other}"))),
    };
    Ok((profile_of(cfg), parts.password.is_some()))
}

pub(crate) fn parse_uri_impl(state: &AppState, uri: &str) -> Result<ParsedConnectionUri> {
    let _ = state;
    let (profile, has_password) = parse_uri(uri)?;
    Ok(ParsedConnectionUri {
        profile,
        has_password,
    })
}

#[tauri::command]
pub fn connection_parse_uri(
    state: State<'_, AppState>,
    uri: String,
) -> Result<ParsedConnectionUri> {
    tracing::debug!(command = "connection_parse_uri", "ipc call");
    parse_uri_impl(&state, &uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mongodb_uri_with_options() {
        let (p, s) = parse_uri(
            "mongodb://dbstudio:p%40ss%2Fw%3Ard@h1:27017,h2:27018/app?authSource=admin&replicaSet=rs0&directConnection=true&tls=true&tlsCAFile=/ca.pem&appName=x&unknown=1",
        )
        .unwrap();
        let c = p.config;
        assert_eq!(
            (c.kind, c.host.as_str(), c.port),
            (DbKind::MongoDb, "h1", 27017)
        );
        assert_eq!(c.database.as_deref(), Some("app"));
        assert_eq!(c.username.as_deref(), Some("dbstudio"));
        assert!(s, "password reported, not kept");
        assert_eq!(c.options["authSource"], "admin");
        assert_eq!(c.options["replicaSet"], "rs0");
        assert_eq!(c.options["directConnection"], "true");
        assert_eq!(c.options["hosts"], "h1:27017,h2:27018");
        assert!(!c.options.contains_key("unknown"));
        assert_eq!(c.tls.mode, TlsMode::VerifyFull);
        assert_eq!(c.tls.ca_cert_path.as_deref(), Some("/ca.pem"));
        assert!(
            !p.has_password && !p.save_password,
            "no password in the profile"
        );
    }

    #[test]
    fn mongodb_srv_defaults_to_tls() {
        let (p, s) = parse_uri("mongodb+srv://u@cluster0.example.net/?retryWrites=true").unwrap();
        assert!(!s);
        assert_eq!(p.config.options["srv"], "true");
        assert_eq!(p.config.tls.mode, TlsMode::VerifyFull);
        assert!(p.config.database.is_none());
        let (p, _) = parse_uri("mongodb://h/?tls=true&tlsAllowInvalidCertificates=true").unwrap();
        assert_eq!(p.config.tls.mode, TlsMode::Require);
        let (p, _) = parse_uri("mongodb://h/?tls=false").unwrap();
        assert_eq!(p.config.tls.mode, TlsMode::Disable);
        let (p, _) = parse_uri("mongodb://h").unwrap();
        assert_eq!((p.config.tls.mode, p.config.port), (TlsMode::Prefer, 27017));
    }

    #[test]
    fn redis_and_sql_uris() {
        let (p, s) = parse_uri("rediss://:secret@cache.example.com:6380/2").unwrap();
        assert_eq!(p.config.kind, DbKind::Redis);
        assert_eq!(
            (p.config.port, p.config.database.as_deref()),
            (6380, Some("2"))
        );
        assert!(p.config.username.is_none());
        assert!(s);
        assert_eq!(p.config.tls.mode, TlsMode::VerifyFull);
        assert!(parse_uri("redis://h/notanumber").is_err());

        let (p, s) =
            parse_uri("postgres://me:pw@db.local/shop?sslmode=verify-full&sslrootcert=/r.pem")
                .unwrap();
        assert_eq!((p.config.kind, p.config.port), (DbKind::Postgres, 5432));
        assert_eq!(p.config.tls.mode, TlsMode::VerifyFull);
        assert_eq!(p.config.tls.ca_cert_path.as_deref(), Some("/r.pem"));
        assert!(s);
        let (p, _) = parse_uri("mysql://root@127.0.0.1:3307/app?ssl-mode=REQUIRED").unwrap();
        assert_eq!(
            (p.config.kind, p.config.port, p.config.tls.mode),
            (DbKind::MySql, 3307, TlsMode::Require)
        );
        let (p, _) = parse_uri(
            "sqlserver://sa:pw@h:14333;database=master;encrypt=true;trustServerCertificate=true",
        )
        .unwrap();
        assert_eq!((p.config.kind, p.config.port), (DbKind::SqlServer, 14333));
        assert_eq!(p.config.database.as_deref(), Some("master"));
        assert_eq!(p.config.tls.mode, TlsMode::Require);
        let (p, _) = parse_uri("oracle://SYSTEM@h/FREEPDB1").unwrap();
        assert_eq!((p.config.kind, p.config.port), (DbKind::Oracle, 1521));
        assert_eq!(p.config.database.as_deref(), Some("FREEPDB1"));
        let (p, _) = parse_uri("postgres://u@[::1]:5433/d").unwrap();
        assert_eq!((p.config.host.as_str(), p.config.port), ("::1", 5433));
    }

    #[test]
    fn errors_do_not_echo_the_uri() {
        let e = parse_uri("mongodb://u:topsecret@:bad/x").unwrap_err();
        assert!(!e.message().contains("topsecret"), "{e}");
        assert_eq!(parse_uri("ftp://h").unwrap_err().kind(), "unsupported");
        assert_eq!(parse_uri("not a uri").unwrap_err().kind(), "invalid");
        assert_eq!(parse_uri("mongodb://").unwrap_err().kind(), "invalid");
        assert!(parse_uri("mongodb://u:p%zz@h").is_err());
    }

    #[test]
    fn password_never_reaches_the_frontend() {
        let (profile, has_password) = parse_uri("mongodb://u:pw@h/x").unwrap();
        assert!(has_password);
        let json = serde_json::to_string(&ParsedConnectionUri {
            profile,
            has_password,
        })
        .unwrap();
        assert!(!json.contains("\"pw\"") && json.contains("\"hasPassword\":true"));
    }

    #[test]
    fn ipv6_hosts_keep_brackets_and_slash_in_password_stays_in_userinfo() {
        let (p, _) = parse_uri("mongodb://u:p@[::1]:27017,[fe80::2]:27018/d").unwrap();
        assert_eq!(p.config.host, "::1");
        assert_eq!(p.config.options["hosts"], "[::1]:27017,[fe80::2]:27018");
        let (p, has) =
            parse_uri("mongodb://user:pa/ss@db.example.com:27018/app?authSource=admin").unwrap();
        assert!(has);
        assert_eq!(
            (p.config.host.as_str(), p.config.port),
            ("db.example.com", 27018)
        );
        assert_eq!(p.config.database.as_deref(), Some("app"));
        assert_eq!(p.config.username.as_deref(), Some("user"));
    }

    #[test]
    fn newsql_uris_use_their_own_kinds_and_default_ports() {
        let (p, s) = parse_uri(
            "cockroachdb://root:pw@crdb.example.com/defaultdb?sslmode=verify-full&sslrootcert=/ca.crt&options=--cluster%3Dmy-cluster-123",
        )
        .unwrap();
        let c = p.config;
        assert_eq!((c.kind, c.port), (DbKind::CockroachDb, 26257));
        assert_eq!(c.database.as_deref(), Some("defaultdb"));
        assert_eq!(c.tls.mode, TlsMode::VerifyFull);
        assert_eq!(c.options["cockroach.cluster"], "my-cluster-123");
        assert!(s);
        let (p, _) = parse_uri("cockroachdb://u@h/db?cluster=abc").unwrap();
        assert_eq!(p.config.options["cockroach.cluster"], "abc");
        let (p, _) = parse_uri("yugabytedb://yugabyte@yb:5433/yugabyte?sslmode=require").unwrap();
        assert_eq!(
            (p.config.kind, p.config.port, p.config.tls.mode),
            (DbKind::YugabyteDb, 5433, TlsMode::Require)
        );
        let (p, _) = parse_uri("yugabyte://u@yb/db").unwrap();
        assert_eq!(p.config.kind, DbKind::YugabyteDb);
        let (p, s) = parse_uri("tidb://root@tidb.local/test?ssl-mode=REQUIRED").unwrap();
        assert_eq!(
            (p.config.kind, p.config.port, p.config.tls.mode),
            (DbKind::TiDb, 4000, TlsMode::Require)
        );
        assert!(!s);
    }

    #[test]
    fn clickhouse_uris_pick_http_or_https() {
        let (p, s) = parse_uri("clickhouse://dbstudio:pw@ch.local/analytics?compress=lz4").unwrap();
        let c = p.config;
        assert_eq!(
            (c.kind, c.port, c.tls.mode),
            (DbKind::ClickHouse, 8123, TlsMode::Disable)
        );
        assert_eq!(c.database.as_deref(), Some("analytics"));
        assert_eq!(c.options["compress"], "lz4");
        assert!(s);
        let (p, _) = parse_uri("clickhouses://u@ch.cloud/db").unwrap();
        assert_eq!(
            (p.config.port, p.config.tls.mode),
            (8443, TlsMode::VerifyFull)
        );
        let (p, _) = parse_uri("clickhouse://u@h:9000/db?secure=true&skip_verify=true").unwrap();
        assert_eq!((p.config.port, p.config.tls.mode), (9000, TlsMode::Require));
        let (p, _) = parse_uri("clickhouse://u@h?database=d2").unwrap();
        assert_eq!(p.config.database.as_deref(), Some("d2"));
    }

    #[test]
    fn cassandra_and_scylla_uris_carry_keyspace_and_cluster_options() {
        let (p, s) = parse_uri(
            "cassandra://cass:pw@n1:9042,n2:9043/shop?localDc=dc1&consistency=LOCAL_QUORUM&ssl=true&lwtEdits=true",
        )
        .unwrap();
        let c = p.config;
        assert_eq!((c.kind, c.port), (DbKind::Cassandra, 9042));
        assert_eq!(c.database.as_deref(), Some("shop"));
        assert_eq!(c.options["hosts"], "n1:9042,n2:9043");
        assert_eq!(c.options["localDc"], "dc1");
        assert_eq!(c.options["consistency"], "LOCAL_QUORUM");
        assert_eq!(c.options["lwtEdits"], "true");
        assert_eq!(c.tls.mode, TlsMode::VerifyFull);
        assert!(s);
        let (p, _) = parse_uri("scylladb://h").unwrap();
        assert_eq!(
            (p.config.kind, p.config.port, p.config.tls.mode),
            (DbKind::ScyllaDb, 9042, TlsMode::Prefer)
        );
        let (p, _) = parse_uri("scylla://h/ks").unwrap();
        assert_eq!(p.config.kind, DbKind::ScyllaDb);
        assert_eq!(p.config.database.as_deref(), Some("ks"));
        // The password still never reaches the profile.
        assert!(
            parse_uri("cassandra://u:topsecret@:bad/x")
                .unwrap_err()
                .message()
                .find("topsecret")
                .is_none()
        );
    }
}
