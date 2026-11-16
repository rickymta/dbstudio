//! Connection URI parsing (package K, CONN-13).
//!
//! `connection_parse_uri` turns `mongodb://`, `mongodb+srv://`, `redis://`, `rediss://`,
//! `postgres(ql)://`, `mysql://`, `sqlserver://`, `oracle://`, `cockroachdb://`, `yugabytedb://`,
//! `tidb://`, `clickhouse[s]://`, `cassandra://` and `scylladb://` URIs into a connection profile.
//! JDBC strings (`jdbc:oracle:thin:`, `jdbc:postgresql:`, `jdbc:mysql:`, `jdbc:mariadb:`,
//! `jdbc:sqlserver:`, `jdbc:clickhouse:`, `jdbc:ch:`) give the same profile.
//! ADO.NET connection strings for SQL Server (`Server=host,1433;Database=db;User Id=u;...`) are
//! recognised too (no `scheme://`, `key=value;` pairs with a server key).
//! The password never goes to the frontend and is not kept in memory either: the command
//! reports `hasPassword` and the UI asks the user to type the password again.

use dbstudio_core::{AuthMode, ConnectionConfig, DbKind, Error, TlsMode};
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
    /// Things the parser simplified (extra hosts, SQL Server instance...); shown to the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
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
            "true" | "1" | "yes" | "required" | "strict"
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
    } else if let Some(t) = truthy(q(p, "ssl"))
        .or_else(|| truthy(q(p, "tls")))
        .or_else(|| truthy(q(p, "usessl")))
    {
        cfg.tls.mode = if !t {
            TlsMode::Disable
        } else if truthy(q(p, "verifyservercertificate")).unwrap_or(false) {
            TlsMode::VerifyFull
        } else {
            TlsMode::Require
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
        cfg.connect_timeout_ms = Some(secs.saturating_mul(1000));
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
        cfg.connect_timeout_ms = Some(ms.saturating_mul(1000));
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
        cfg.connect_timeout_ms = Some(ms.saturating_mul(1000));
    }
    Ok(cfg)
}

/// Parse a URI into a profile and whether it carried a password (which is discarded).
#[cfg(test)]
fn parse_uri(uri: &str) -> Result<(ConnectionProfile, bool)> {
    parse_uri_full(uri).map(|(p, has, _)| (p, has))
}

/// Same as [`parse_uri`] plus notes about what was simplified.
fn parse_uri_full(uri: &str) -> Result<(ConnectionProfile, bool, Vec<String>)> {
    let trimmed = uri.trim();
    if trimmed
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("jdbc:"))
    {
        return parse_jdbc(&trimmed[5..]);
    }
    if looks_like_ado(trimmed) {
        let ado = parse_ado_net(trimmed)?;
        let has_password = ado.password.is_some();
        return Ok((ado.profile, has_password, ado.notes));
    }
    let parts = split_uri(trimmed)?;
    let (profile, has_password) = finish(&parts, None)?;
    Ok((profile, has_password, Vec::new()))
}

/// Build the profile of `parts`; `forced_pw` marks a password found outside the user info.
fn finish(parts: &Parts, forced_pw: Option<bool>) -> Result<(ConnectionProfile, bool)> {
    let mut cfg = match parts.scheme.as_str() {
        "mongodb" | "mongodb+srv" => parse_mongo(parts)?,
        "redis" | "rediss" => parse_redis(parts)?,
        "postgres" | "postgresql" | "mysql" | "mariadb" | "sqlserver" | "mssql" | "oracle"
        | "cockroachdb" | "cockroach" | "yugabytedb" | "yugabyte" | "tidb" => parse_sql(parts)?,
        "clickhouse" | "clickhouses" => parse_clickhouse(parts)?,
        "cassandra" | "scylladb" | "scylla" => parse_cassandra(parts)?,
        other => return Err(Error::Unsupported(format!("uri_scheme:{other}"))),
    };
    apply_auth_default(
        &mut cfg,
        parts.password.is_some() || forced_pw == Some(true),
    );
    Ok((
        profile_of(cfg),
        parts.password.is_some() || forced_pw == Some(true),
    ))
}

/// A URI without user and password points at a server that most likely needs no login
/// (local Redis, MongoDB, Cassandra, ClickHouse, PostgreSQL trust). Servers that always need
/// a user (MySQL family, SQL Server, Oracle) keep the password mode.
fn apply_auth_default(cfg: &mut ConnectionConfig, has_password: bool) {
    if !has_password
        && cfg.username.is_none()
        && matches!(
            cfg.kind,
            DbKind::Redis
                | DbKind::MongoDb
                | DbKind::Cassandra
                | DbKind::ScyllaDb
                | DbKind::ClickHouse
                | DbKind::Postgres
                | DbKind::CockroachDb
                | DbKind::YugabyteDb
        )
    {
        cfg.auth_mode = AuthMode::NoAuth;
    }
}

const JDBC_HINT: &str = "supported JDBC forms: jdbc:oracle:thin:@//host:1521/service, \
jdbc:oracle:thin:@host:1521:SID, jdbc:postgresql://host/db, jdbc:mysql://host/db, \
jdbc:mariadb://host/db, jdbc:sqlserver://host;databaseName=db, jdbc:clickhouse://host:8123/db";

fn jdbc_invalid(msg: &str) -> Error {
    Error::Invalid(format!("invalid JDBC URL: {msg}; {JDBC_HINT}"))
}

/// `rest` is the text after `jdbc:`.
fn parse_jdbc(rest: &str) -> Result<(ConnectionProfile, bool, Vec<String>)> {
    let (sub, tail) = rest
        .split_once(':')
        .ok_or_else(|| jdbc_invalid("missing subprotocol"))?;
    let sub = sub.to_ascii_lowercase();
    if sub == "oracle" {
        let thin = tail
            .get(..5)
            .filter(|t| t.eq_ignore_ascii_case("thin:"))
            .map(|_| &tail[5..])
            .ok_or_else(|| jdbc_invalid("only the Oracle thin driver is supported"))?;
        return parse_jdbc_oracle(thin);
    }
    let scheme = match sub.as_str() {
        "postgresql" | "mysql" | "mariadb" | "sqlserver" => sub.clone(),
        "clickhouse" | "ch" => "clickhouse".to_string(),
        other => return Err(jdbc_invalid(&format!("unsupported subprotocol `{other}`"))),
    };
    let mut body = tail;
    let mut scheme = scheme;
    if scheme == "clickhouse" {
        // `jdbc:ch:http://...` / `jdbc:clickhouse:https://...` name the protocol first.
        for (prefix, s) in [("https://", "clickhouses"), ("http://", "clickhouse")] {
            if let Some(b) = body
                .get(..prefix.len())
                .filter(|h| h.eq_ignore_ascii_case(prefix))
                .map(|_| &body[prefix.len()..])
            {
                body = b;
                scheme = s.to_string();
                break;
            }
        }
        if body.starts_with("//") {
            body = &body[2..];
        }
    } else {
        body = body
            .strip_prefix("//")
            .ok_or_else(|| jdbc_invalid("expected `//host` after the subprotocol"))?;
    }
    let mut parts = split_uri(&format!("{scheme}://{body}"))
        .map_err(|e| jdbc_invalid(&e.to_string().replace("invalid connection URI: ", "")))?;
    let mut notes = Vec::new();
    if parts.hosts.len() > 1 {
        notes.push(format!(
            "The URL lists {} hosts; only the first one ({}) was used.",
            parts.hosts.len(),
            parts.hosts[0].0
        ));
    }
    let mut instance = None;
    if scheme == "sqlserver" {
        let host = parts.hosts[0].0.clone();
        if let Some((h, inst)) = host.split_once('\\') {
            instance = Some(inst.to_string());
            parts.hosts[0].0 = h.to_string();
        }
    }
    let url_pw = q(&parts, "password")
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    if parts.password.is_none() {
        parts.password.clone_from(&url_pw);
    }
    let (mut profile, has_password) = finish(&parts, None)?;
    if let Some(inst) = instance.filter(|i| !i.is_empty()) {
        profile
            .config
            .options
            .insert("instance".into(), inst.clone());
        notes.push(format!(
            "SQL Server instance `{inst}` kept as an option; set the port if the instance listens on a fixed one."
        ));
    }
    Ok((profile, has_password, notes))
}

/// A parsed ADO.NET connection string. The password is kept only for callers that connect
/// right away (tests); `connection_parse_uri` reports just `has_password`. No `Debug` on
/// purpose: the secret must not end up in a log.
pub(crate) struct AdoParsed {
    pub profile: ConnectionProfile,
    pub password: Option<String>,
    pub notes: Vec<String>,
}

/// Normalised key: lower case without spaces, underscores or dashes.
fn ado_key(k: &str) -> String {
    k.chars()
        .filter(|c| !c.is_whitespace() && *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

const ADO_SERVER_KEYS: [&str; 5] = ["server", "datasource", "address", "addr", "networkaddress"];

/// Split `key=value;key=value` following the ADO.NET rules: values may be quoted with `"` or
/// `'` (a doubled quote is a literal one) or wrapped in `{}` (`}}` is a literal `}`), and `;`
/// is allowed inside quotes. Returns `None` for text that is not a list of pairs.
fn ado_pairs(s: &str) -> Option<Vec<(String, String)>> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ';') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        while i < chars.len() && chars[i] != '=' && chars[i] != ';' {
            i += 1;
        }
        if i >= chars.len() || chars[i] != '=' {
            return None;
        }
        let key: String = chars[start..i].iter().collect();
        let key = key.trim().to_string();
        if key.is_empty() {
            return None;
        }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        match chars.get(i).copied() {
            Some(q @ ('"' | '\'')) => {
                i += 1;
                loop {
                    match chars.get(i).copied() {
                        None => return None,
                        Some(c) if c == q => {
                            if chars.get(i + 1) == Some(&q) {
                                value.push(q);
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(c) => {
                            value.push(c);
                            i += 1;
                        }
                    }
                }
            }
            Some('{') => {
                i += 1;
                loop {
                    match chars.get(i).copied() {
                        None => return None,
                        Some('}') => {
                            if chars.get(i + 1) == Some(&'}') {
                                value.push('}');
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(c) => {
                            value.push(c);
                            i += 1;
                        }
                    }
                }
            }
            _ => {
                while i < chars.len() && chars[i] != ';' {
                    value.push(chars[i]);
                    i += 1;
                }
                value = value.trim().to_string();
            }
        }
        // Only blanks may follow a quoted value before the `;`.
        while i < chars.len() && chars[i] != ';' {
            if !chars[i].is_whitespace() {
                return None;
            }
            i += 1;
        }
        out.push((key, value));
    }
    Some(out)
}

/// True for an ADO.NET string: no `scheme://`, no `jdbc:`, and `key=value;` pairs naming a server.
fn looks_like_ado(s: &str) -> bool {
    if s.contains("://") || s.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("jdbc:")) {
        return false;
    }
    ado_pairs(s).is_some_and(|pairs| {
        pairs
            .iter()
            .any(|(k, _)| ADO_SERVER_KEYS.contains(&ado_key(k).as_str()))
    })
}

fn ado_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "sspi" | "mandatory" | "strict" => Some(true),
        "false" | "no" | "0" | "optional" => Some(false),
        _ => None,
    }
}

/// `host`, `host,port`, `tcp:host,port`, `host\INSTANCE[,port]`, `[::1],1433`.
fn ado_server(
    value: &str,
    notes: &mut Vec<String>,
) -> Result<(String, Option<u16>, Option<String>)> {
    let mut v = value.trim();
    for prefix in ["tcp:", "lpc:"] {
        if v.get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
        {
            v = &v[prefix.len()..];
        }
    }
    if v.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("np:")) || v.starts_with("\\\\") {
        return Err(invalid("named pipes are not supported; use a TCP host"));
    }
    let (addr, port) = match v.rsplit_once(',') {
        Some((a, p)) if !a.ends_with(']') || a.contains(']') => {
            let port = p
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| invalid("invalid port in the server address"))?;
            (a.trim(), Some(port))
        }
        _ => (v, None),
    };
    let (host, instance) = match addr.split_once('\\') {
        Some((h, i)) => (
            h.trim(),
            Some(i.trim().to_string()).filter(|i| !i.is_empty()),
        ),
        None => (addr, None),
    };
    let host = match host.to_ascii_lowercase().as_str() {
        "" | "." | "(local)" | "localhost" => "localhost".to_string(),
        _ => host.to_string(),
    };
    if let Some(inst) = &instance {
        if port.is_none() {
            notes.push(format!(
                "SQL Server instance `{inst}` kept as an option, but named instances need an explicit port (the SQL Browser lookup is not used): add `,port` to the server."
            ));
        }
    }
    Ok((host, port, instance))
}

/// Parse an ADO.NET connection string for SQL Server. Error messages never echo the input.
pub(crate) fn parse_ado_net(s: &str) -> Result<AdoParsed> {
    let pairs = ado_pairs(s).ok_or_else(|| invalid("malformed ADO.NET connection string"))?;
    let mut notes = Vec::new();
    let mut server = None;
    let mut database = None;
    let mut user = None;
    let mut password = None;
    let mut encrypt: Option<String> = None;
    let mut trust = None;
    let mut integrated = false;
    let mut app = None;
    let mut timeout = None;
    let mut cert_host = None;
    let mut pool_max = None;
    for (k, v) in pairs {
        let key = ado_key(&k);
        match key.as_str() {
            "server" | "datasource" | "address" | "addr" | "networkaddress" => server = Some(v),
            "database" | "initialcatalog" => database = Some(v),
            "userid" | "uid" | "user" | "username" => user = Some(v),
            "password" | "pwd" => password = Some(v),
            "encrypt" => encrypt = Some(v.trim().to_ascii_lowercase()),
            "trustservercertificate" => match ado_bool(&v) {
                Some(b) => trust = Some(b),
                None => notes.push("TrustServerCertificate has an unknown value; ignored.".into()),
            },
            "integratedsecurity" | "trustedconnection" => match ado_bool(&v) {
                Some(b) => integrated = b,
                None => notes.push(format!("{k} has an unknown value; ignored.")),
            },
            "applicationname" | "app" => app = Some(v),
            "connecttimeout" | "connectiontimeout" | "timeout" => match v.trim().parse::<u64>() {
                Ok(secs) => timeout = Some(secs.saturating_mul(1000)),
                Err(_) => notes.push(format!("{k} is not a number of seconds; ignored.")),
            },
            "hostnameincertificate" => cert_host = Some(v),
            "maxpoolsize" => match v.trim().parse::<usize>() {
                Ok(n) => pool_max = Some(n.clamp(1, 32)),
                Err(_) => notes.push(format!("{k} is not a number; ignored.")),
            },
            // Harmless for DBStudio: accepted without a note.
            "multipleactiveresultsets" | "persistsecurityinfo" | "pooling" => {}
            _ => notes.push(format!("`{k}` is not supported and was ignored.")),
        }
    }
    let server = server
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| invalid("the ADO.NET string has no Server / Data Source"))?;
    let (host, port, instance) = ado_server(&server, &mut notes)?;
    let database = database.filter(|d| !d.is_empty());
    let label = match &database {
        Some(d) => format!("{host}/{d}"),
        None => host.clone(),
    };
    let mut cfg = ConnectionConfig::new(DbKind::SqlServer, label, host);
    cfg.port = port.unwrap_or(1433);
    cfg.database = database;
    cfg.username = user.filter(|u| !u.is_empty());
    cfg.connect_timeout_ms = timeout;
    if let Some(i) = instance {
        cfg.options.insert("instance".into(), i);
    }
    if integrated {
        cfg.options.insert("auth".into(), "integrated".into());
    }
    if let Some(a) = app.filter(|a| !a.is_empty()) {
        cfg.options.insert("application_name".into(), a);
    }
    if let Some(n) = pool_max {
        cfg.options.insert("pool_max".into(), n.to_string());
    }
    if let Some(h) = cert_host.filter(|h| !h.is_empty()) {
        cfg.tls.server_name = Some(h);
    }
    // Same mapping as `parse_sql` for `jdbc:sqlserver` (see `apply_tls` of the mssql driver):
    // Encrypt=false -> Disable (the login stays encrypted), true -> Require with
    // TrustServerCertificate else VerifyFull. Without `Encrypt` the profile keeps the default
    // (Prefer), exactly like the JDBC parser; a trusted certificate alone means Require.
    let trusted = trust.unwrap_or(false);
    match encrypt.as_deref() {
        Some("strict") => {
            cfg.options.insert("encrypt".into(), "strict".into());
            cfg.tls.mode = if trusted {
                TlsMode::Require
            } else {
                TlsMode::VerifyFull
            };
        }
        Some(e) => match ado_bool(e) {
            Some(false) => cfg.tls.mode = TlsMode::Disable,
            Some(true) => {
                cfg.tls.mode = if trusted {
                    TlsMode::Require
                } else {
                    TlsMode::VerifyFull
                };
            }
            None => notes.push("Encrypt has an unknown value; ignored.".into()),
        },
        None => {
            if trusted {
                cfg.tls.mode = TlsMode::Require;
            }
        }
    }
    let password = password.filter(|p| !p.is_empty());
    Ok(AdoParsed {
        profile: profile_of(cfg),
        password,
        notes,
    })
}

fn split_hp(h: &str) -> Result<(String, Option<u16>)> {
    if h.is_empty() {
        return Err(jdbc_invalid("missing host"));
    }
    split_host_port(h).map_err(|_| jdbc_invalid("bad host or port"))
}

/// Value of `(KEY=value)` in a TNS descriptor (key matched without case, spaces ignored).
fn tns_value(desc: &str, key: &str) -> Option<String> {
    let upper = desc.to_ascii_uppercase();
    let needle = format!("({}=", key.to_ascii_uppercase());
    let start = upper.find(&needle)? + needle.len();
    let end = desc[start..].find(')')? + start;
    Some(desc[start..end].trim().to_string()).filter(|v| !v.is_empty())
}

fn parse_jdbc_oracle(thin: &str) -> Result<(ConnectionProfile, bool, Vec<String>)> {
    let (cred, addr) = thin
        .rsplit_once('@')
        .ok_or_else(|| jdbc_invalid("expected `@` before the Oracle address"))?;
    let (user, password) = match cred.split_once('/') {
        Some((u, p)) => (u, Some(p)),
        None => (cred, None),
    };
    let user = Some(user.trim().to_string()).filter(|u| !u.is_empty());
    let password = password.map(str::to_string).filter(|p| !p.is_empty());
    let addr = addr.trim();
    if addr.is_empty() {
        return Err(jdbc_invalid("the Oracle address is empty"));
    }
    let mut tls = false;
    let (host, port, service, sid);
    if addr.starts_with('(') {
        let flat: String = addr.chars().filter(|c| !c.is_whitespace()).collect();
        let upper = flat.to_ascii_uppercase();
        if upper.matches("(ADDRESS=").count() != 1
            || [
                "ADDRESS_LIST",
                "FAILOVER",
                "LOAD_BALANCE",
                "(DESCRIPTION_LIST",
            ]
            .iter()
            .any(|k| upper.contains(k))
        {
            return Err(jdbc_invalid(
                "this TNS descriptor has several addresses or failover; use a single (ADDRESS=...) or fill the fields by hand",
            ));
        }
        let h =
            tns_value(&flat, "HOST").ok_or_else(|| jdbc_invalid("TNS descriptor has no HOST"))?;
        host = h;
        port = match tns_value(&flat, "PORT") {
            Some(p) => Some(p.parse::<u16>().map_err(|_| jdbc_invalid("bad PORT"))?),
            None => None,
        };
        service = tns_value(&flat, "SERVICE_NAME");
        sid = tns_value(&flat, "SID");
        tls = tns_value(&flat, "PROTOCOL").is_some_and(|p| p.eq_ignore_ascii_case("TCPS"));
        if service.is_none() && sid.is_none() {
            return Err(jdbc_invalid("TNS descriptor has no SERVICE_NAME or SID"));
        }
    } else {
        let mut a = addr;
        for (pre, t) in [("tcps://", true), ("tcp://", false), ("//", false)] {
            if let Some(r) = a.strip_prefix(pre) {
                a = r;
                tls = t;
                break;
            }
        }
        let a = a.split('?').next().unwrap_or(a);
        if let Some((hp, svc)) = a.split_once('/') {
            let (h, p) = split_hp(hp)?;
            host = h;
            port = p;
            service = Some(svc.trim().to_string()).filter(|s| !s.is_empty());
            sid = None;
            if service.is_none() {
                return Err(jdbc_invalid("missing service name"));
            }
        } else if !a.starts_with('[') && a.matches(':').count() == 2 {
            // Legacy `host:port:SID`.
            let mut it = a.splitn(3, ':');
            let (h, p, s) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next());
            if h.is_empty() || s.is_none_or(str::is_empty) {
                return Err(jdbc_invalid("expected host:port:SID"));
            }
            host = h.to_string();
            port = Some(p.parse::<u16>().map_err(|_| jdbc_invalid("bad port"))?);
            service = None;
            sid = s.map(str::to_string);
        } else {
            return Err(jdbc_invalid("missing service name or SID"));
        }
    }
    let port = port.unwrap_or(1521);
    let db = service.clone();
    let label = db.clone().or_else(|| sid.clone()).unwrap_or_default();
    let mut cfg = ConnectionConfig::new(DbKind::Oracle, format!("{host}/{label}"), host);
    cfg.port = port;
    cfg.database = db;
    cfg.username = user;
    let mut notes = Vec::new();
    if let Some(s) = sid.filter(|_| service.is_none()) {
        cfg.options.insert("sid".into(), s.clone());
        notes.push(format!(
            "Oracle SID `{s}` was stored in the `sid` option (the service name field is empty)."
        ));
    }
    if tls {
        cfg.tls.mode = TlsMode::VerifyFull;
    }
    let has_password = password.is_some();
    Ok((profile_of(cfg), has_password, notes))
}

pub(crate) fn parse_uri_impl(state: &AppState, uri: &str) -> Result<ParsedConnectionUri> {
    let _ = state;
    let (profile, has_password, notes) = parse_uri_full(uri)?;
    Ok(ParsedConnectionUri {
        profile,
        has_password,
        notes,
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
    fn uri_without_user_and_password_means_no_authentication() {
        let (p, _) = parse_uri("redis://cache:6379/0").unwrap();
        assert_eq!(p.config.auth_mode, AuthMode::NoAuth);
        let (p, _) = parse_uri("mongodb://h/").unwrap();
        assert_eq!(p.config.auth_mode, AuthMode::NoAuth);
        let (p, _) = parse_uri("redis://:secret@cache").unwrap();
        assert_eq!(p.config.auth_mode, AuthMode::Password);
        let (p, _) = parse_uri("postgres://me@db/app").unwrap();
        assert_eq!(p.config.auth_mode, AuthMode::Password);
        let (p, _) = parse_uri("mysql://db/app").unwrap();
        assert_eq!(p.config.auth_mode, AuthMode::Password);
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
            notes: Vec::new(),
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
    fn huge_timeouts_do_not_overflow() {
        let (p, _) = parse_uri("postgres://u@h/d?connect_timeout=18446744073709551615").unwrap();
        assert_eq!(p.config.connect_timeout_ms, Some(u64::MAX));
        let (p, _) = parse_uri("clickhouse://u@h/d?connect_timeout=18446744073709551615").unwrap();
        assert_eq!(p.config.connect_timeout_ms, Some(u64::MAX));
        let (p, _) = parse_uri("cassandra://u@h/ks?connect_timeout=18446744073709551615").unwrap();
        assert_eq!(p.config.connect_timeout_ms, Some(u64::MAX));
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

    #[test]
    fn jdbc_oracle_user_string() {
        let (p, s, n) = parse_uri_full("jdbc:oracle:thin:@//localhost:1521/FREEPDB1").unwrap();
        let c = p.config;
        assert_eq!(
            (c.kind, c.host.as_str(), c.port),
            (DbKind::Oracle, "localhost", 1521)
        );
        assert_eq!(c.database.as_deref(), Some("FREEPDB1"));
        assert!(!s && n.is_empty() && !c.options.contains_key("sid"));
        assert_eq!(c.auth_mode, AuthMode::Password);
    }

    #[test]
    fn jdbc_oracle_variants() {
        // default port, case-insensitive prefix
        let (p, ..) = parse_uri_full("JDBC:Oracle:THIN:@//db.example/svc").unwrap();
        assert_eq!(
            (p.config.port, p.config.database.as_deref()),
            (1521, Some("svc"))
        );
        // host:port/service
        let (p, ..) = parse_uri_full("jdbc:oracle:thin:@h:1522/svc").unwrap();
        assert_eq!(
            (p.config.port, p.config.database.as_deref()),
            (1522, Some("svc"))
        );
        // SID
        let (p, _, n) = parse_uri_full("jdbc:oracle:thin:@h:1521:ORCL").unwrap();
        assert_eq!(p.config.options["sid"], "ORCL");
        assert!(p.config.database.is_none());
        assert_eq!(n.len(), 1);
        // user/password
        let (p, s, _) = parse_uri_full("jdbc:oracle:thin:scott/ti%ger@//h:1521/svc").unwrap();
        assert_eq!(p.config.username.as_deref(), Some("scott"));
        assert!(s);
        assert!(!p.has_password);
        // tcps
        let (p, ..) = parse_uri_full("jdbc:oracle:thin:@tcps://h:2484/svc").unwrap();
        assert_eq!(
            (p.config.port, p.config.tls.mode),
            (2484, TlsMode::VerifyFull)
        );
        // TNS
        let (p, ..) = parse_uri_full(
            "jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=h)(PORT=1530))(CONNECT_DATA=(SERVICE_NAME=s)))",
        )
        .unwrap();
        assert_eq!(
            (
                p.config.host.as_str(),
                p.config.port,
                p.config.database.as_deref()
            ),
            ("h", 1530, Some("s"))
        );
        let (p, ..) = parse_uri_full(
            "jdbc:oracle:thin:@(DESCRIPTION = (ADDRESS = (PROTOCOL = TCPS)(HOST = h)(PORT = 2484))(CONNECT_DATA = (SID = X)))",
        )
        .unwrap();
        assert_eq!(p.config.options["sid"], "X");
        assert_eq!(p.config.tls.mode, TlsMode::VerifyFull);
    }

    #[test]
    fn jdbc_oracle_errors() {
        for bad in [
            "jdbc:oracle:thin:@",
            "jdbc:oracle:thin://h/svc",
            "jdbc:oracle:oci:@h:1521:X",
            "jdbc:oracle:thin:@//h:1521",
            "jdbc:oracle:thin:@//h:notaport/svc",
            "jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(HOST=a)(PORT=1))(ADDRESS=(HOST=b)(PORT=1))(CONNECT_DATA=(SERVICE_NAME=s)))",
            "jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(HOST=a)(PORT=1)))",
        ] {
            let e = parse_uri(bad).unwrap_err();
            assert!(matches!(e, Error::Invalid(_)), "{bad}");
            assert!(e.to_string().contains("jdbc:oracle:thin"), "{bad}");
        }
    }

    #[test]
    fn jdbc_other_databases() {
        let (p, s, n) = parse_uri_full(
            "jdbc:postgresql://h1:6432,h2/app?user=me&password=p%40&ssl=true&sslmode=verify-full",
        )
        .unwrap();
        let c = p.config;
        assert_eq!(
            (c.kind, c.host.as_str(), c.port),
            (DbKind::Postgres, "h1", 6432)
        );
        assert_eq!(c.database.as_deref(), Some("app"));
        assert_eq!(c.username.as_deref(), Some("me"));
        assert_eq!(c.tls.mode, TlsMode::VerifyFull);
        assert!(s && n.len() == 1);
        let (p, s, _) = parse_uri_full("jdbc:mysql://db:3307/shop?user=u&useSSL=true").unwrap();
        assert_eq!((p.config.kind, p.config.port), (DbKind::MySql, 3307));
        assert_eq!(p.config.tls.mode, TlsMode::Require);
        assert!(!s);
        let (p, ..) = parse_uri_full("jdbc:mariadb://db/shop?useSSL=false").unwrap();
        assert_eq!((p.config.kind, p.config.port), (DbKind::MySql, 3306));
        assert_eq!(p.config.tls.mode, TlsMode::Disable);
        let (p, s, n) = parse_uri_full(
            "jdbc:sqlserver://srv\\SQLEXPRESS:1444;databaseName=sales;user=sa;password=x;encrypt=true;trustServerCertificate=true",
        )
        .unwrap();
        let c = p.config;
        assert_eq!(
            (c.kind, c.host.as_str(), c.port),
            (DbKind::SqlServer, "srv", 1444)
        );
        assert_eq!(c.database.as_deref(), Some("sales"));
        assert_eq!(c.username.as_deref(), Some("sa"));
        assert_eq!(c.tls.mode, TlsMode::Require);
        assert_eq!(c.options["instance"], "SQLEXPRESS");
        assert!(s && n.len() == 1);
        let (p, ..) = parse_uri_full("jdbc:sqlserver://h;database=d").unwrap();
        assert_eq!(
            (p.config.port, p.config.database.as_deref()),
            (1433, Some("d"))
        );
        let (p, ..) = parse_uri_full("jdbc:clickhouse://ch:8123/db").unwrap();
        assert_eq!((p.config.kind, p.config.port), (DbKind::ClickHouse, 8123));
        assert_eq!(p.config.database.as_deref(), Some("db"));
        let (p, ..) = parse_uri_full("jdbc:ch:https://ch/db").unwrap();
        assert_eq!(
            (p.config.port, p.config.tls.mode),
            (8443, TlsMode::VerifyFull)
        );
        let (p, ..) = parse_uri_full("jdbc:ch://ch").unwrap();
        assert_eq!(p.config.host, "ch");
    }

    #[test]
    fn jdbc_errors_are_clear() {
        for bad in [
            "jdbc:",
            "jdbc:foo",
            "jdbc:db2://h/x",
            "jdbc:postgresql:h/db",
            "jdbc:postgresql://",
            "jdbc:mysql://h:xx/db",
        ] {
            let e = parse_uri(bad).unwrap_err();
            assert!(matches!(e, Error::Invalid(_)), "{bad}");
            assert!(e.to_string().contains("supported JDBC forms"), "{bad}");
        }
    }

    #[test]
    fn ado_net_exact_example() {
        let s = "Server=113.20.107.228;Database=Db;User Id=u;Password=p;TrustServerCertificate=yes;Encrypt=false;";
        let (p, has_pw, notes) = parse_uri_full(s).unwrap();
        let c = p.config;
        assert_eq!(
            (c.kind, c.host.as_str(), c.port),
            (DbKind::SqlServer, "113.20.107.228", 1433)
        );
        assert_eq!(c.database.as_deref(), Some("Db"));
        assert_eq!(c.username.as_deref(), Some("u"));
        assert_eq!(c.tls.mode, TlsMode::Disable);
        assert!(has_pw && notes.is_empty());
        assert_eq!(c.name, "113.20.107.228/Db");
        let ado = parse_ado_net(s).unwrap();
        assert_eq!(ado.password.as_deref(), Some("p"));
        assert!(!format!("{:?}", ado.profile).contains("\"p\""));
    }

    #[test]
    fn ado_net_server_forms() {
        let host = |s: &str| {
            let (p, _, n) = parse_uri_full(s).unwrap();
            (p.config.host, p.config.port, p.config.options, n)
        };
        let (h, port, ..) = host("Server=db.local,1444");
        assert_eq!((h.as_str(), port), ("db.local", 1444));
        let (h, port, ..) = host("Data Source=tcp:db.local,1555;");
        assert_eq!((h.as_str(), port), ("db.local", 1555));
        for alias in ["Address", "Addr", "Network Address"] {
            let (h, ..) = host(&format!("{alias}=x1"));
            assert_eq!(h, "x1");
        }
        for local in [".", "(local)", "localhost", "(LOCAL)"] {
            let (h, port, ..) = host(&format!("Server={local}"));
            assert_eq!((h.as_str(), port), ("localhost", 1433));
        }
        let (h, port, opts, n) = host("Server=srv\\SQLEXPRESS");
        assert_eq!((h.as_str(), port), ("srv", 1433));
        assert_eq!(opts["instance"], "SQLEXPRESS");
        assert!(n.iter().any(|m| m.contains("explicit port")));
        let (h, port, opts, n) = host("Server=(local)\\SQL1,1450");
        assert_eq!(
            (h.as_str(), port, opts["instance"].as_str()),
            ("localhost", 1450, "SQL1")
        );
        assert!(n.is_empty());
        let (h, port, ..) = host("Server=[::1],1433");
        assert_eq!((h.as_str(), port), ("[::1]", 1433));
        assert!(parse_uri_full("Server=h,notaport").is_err());
        assert!(parse_uri_full("Server=np:\\\\.\\pipe\\sql\\query").is_err());
        assert!(parse_uri_full("Server=;Database=d").is_err());
    }

    #[test]
    fn ado_net_keys_and_case() {
        let (p, has_pw, n) = parse_uri_full(
            " SERVER = h ; initial catalog = shop ; UID = sa ; PWD = x ; Application Name=dbs ; \
             Connect Timeout=15 ; Host Name In Certificate=cert.example.com ; Max Pool Size=99 ; \
             Integrated Security=SSPI ; MultipleActiveResultSets=true ; Persist Security Info=false ; Pooling=true ",
        )
        .unwrap();
        let c = p.config;
        assert_eq!(c.database.as_deref(), Some("shop"));
        assert_eq!(c.username.as_deref(), Some("sa"));
        assert_eq!(c.connect_timeout_ms, Some(15_000));
        assert_eq!(c.tls.server_name.as_deref(), Some("cert.example.com"));
        assert_eq!(c.options["pool_max"], "32");
        assert_eq!(c.options["auth"], "integrated");
        assert_eq!(c.options["application_name"], "dbs");
        assert!(has_pw && n.is_empty(), "{n:?}");
        for (k, v) in [("Connection Timeout", "7"), ("Timeout", "7")] {
            let (p, ..) = parse_uri_full(&format!("Server=h;{k}={v}")).unwrap();
            assert_eq!(p.config.connect_timeout_ms, Some(7000));
        }
        let (p, ..) = parse_uri_full("Server=h;Max Pool Size=0;User=u1").unwrap();
        assert_eq!(p.config.options["pool_max"], "1");
        assert_eq!(p.config.username.as_deref(), Some("u1"));
        let (p, ..) = parse_uri_full("Server=h;User Name=u2;Trusted_Connection=yes").unwrap();
        assert_eq!(p.config.username.as_deref(), Some("u2"));
        assert_eq!(p.config.options["auth"], "integrated");
        let (p, ..) = parse_uri_full("Server=h;Integrated Security=false").unwrap();
        assert!(!p.config.options.contains_key("auth"));
    }

    #[test]
    fn ado_net_tls_mapping() {
        let mode = |s: &str| parse_uri_full(s).unwrap().0.config.tls.mode;
        assert_eq!(mode("Server=h;Encrypt=false"), TlsMode::Disable);
        assert_eq!(
            mode("Server=h;Encrypt=no;TrustServerCertificate=true"),
            TlsMode::Disable
        );
        assert_eq!(mode("Server=h;Encrypt=Optional"), TlsMode::Disable);
        assert_eq!(
            mode("Server=h;Encrypt=true;TrustServerCertificate=yes"),
            TlsMode::Require
        );
        assert_eq!(
            mode("Server=h;Encrypt=Mandatory;TrustServerCertificate=true"),
            TlsMode::Require
        );
        assert_eq!(mode("Server=h;Encrypt=true"), TlsMode::VerifyFull);
        assert_eq!(
            mode("Server=h;Encrypt=yes;TrustServerCertificate=false"),
            TlsMode::VerifyFull
        );
        // Without Encrypt: like the JDBC parser, keep the default unless the certificate is trusted.
        assert_eq!(mode("Server=h"), TlsMode::Prefer);
        assert_eq!(
            mode("Server=h;TrustServerCertificate=true"),
            TlsMode::Require
        );
        let (p, ..) = parse_uri_full("Server=h;Encrypt=strict").unwrap();
        assert_eq!(p.config.tls.mode, TlsMode::VerifyFull);
        assert_eq!(p.config.options["encrypt"], "strict");
        let (p, ..) =
            parse_uri_full("Server=h;Encrypt=Strict;TrustServerCertificate=true").unwrap();
        assert_eq!(p.config.tls.mode, TlsMode::Require);
        let (_, _, n) = parse_uri_full("Server=h;Encrypt=maybe").unwrap();
        assert!(n.iter().any(|m| m.contains("Encrypt")));
    }

    #[test]
    fn ado_net_quoting_and_secrets() {
        let ado = parse_ado_net(
            r#"Server=h;Password="a;b=c""d";User Id='o''neil';Database={we;ird}}db}"#,
        )
        .unwrap();
        assert_eq!(ado.password.as_deref(), Some("a;b=c\"d"));
        assert_eq!(ado.profile.config.username.as_deref(), Some("o'neil"));
        assert_eq!(ado.profile.config.database.as_deref(), Some("we;ird}db"));
        let ado = parse_ado_net("Server=h;Password={p}}w;d};").unwrap();
        assert_eq!(ado.password.as_deref(), Some("p}w;d"));
        // Value with `=` inside, unquoted, and an empty password.
        let ado = parse_ado_net("Server=h;Password=ab=cd;").unwrap();
        assert_eq!(ado.password.as_deref(), Some("ab=cd"));
        let (_, has_pw, _) = parse_uri_full("Server=h;Password=;User Id=u").unwrap();
        assert!(!has_pw);
        let (_, has_pw, _) = parse_uri_full("Server=h;User Id=u").unwrap();
        assert!(!has_pw);
    }

    #[test]
    fn ado_net_unknown_keys_warn_and_errors_do_not_leak() {
        let (_, _, n) =
            parse_uri_full("Server=h;Failover Partner=x;Workstation ID=w;Password=hunter2")
                .unwrap();
        assert_eq!(n.len(), 2);
        assert!(n.iter().all(|m| m.contains("ignored")));
        assert!(!n.join(" ").contains("hunter2"));
        for bad in [
            "Server=h;Password=\"unterminated",
            "Database=d;Password=hunter2",
            "Server=h;Password=hunter2;junk",
        ] {
            let msg = parse_uri_full(bad).map(|_| ()).unwrap_err().to_string();
            assert!(!msg.contains("hunter2"), "{bad}");
        }
        // Not ADO.NET: other formats are untouched.
        assert!(!looks_like_ado("postgres://u:p@h/db?x=1"));
        assert!(!looks_like_ado("jdbc:sqlserver://h;databaseName=d"));
        assert!(!looks_like_ado("Database=d;User Id=u"));
    }
}
