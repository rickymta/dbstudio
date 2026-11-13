//! Connection URI parsing (package K, CONN-13).
//!
//! `connection_parse_uri` turns `mongodb://`, `mongodb+srv://`, `redis://`, `rediss://`,
//! `postgres(ql)://`, `mysql://`, `sqlserver://` and `oracle://` URIs into a connection profile.
//! The password never goes to the frontend: it is kept in a small in-memory table and the
//! command returns `pendingSecretId`; [`take_pending_secret`] hands it to the save path.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dbstudio_core::{ConnectionConfig, DbKind, Error, Secret, TlsMode};
use dbstudio_store::ConnectionProfile;
use serde::Serialize;
use tauri::State;
use uuid::Uuid;

use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

/// Result of `connection_parse_uri`; the password stays in the backend (`pending_secret_id`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedConnectionUri {
    pub profile: ConnectionProfile,
    pub has_password: bool,
    pub pending_secret_id: Option<String>,
}

const PENDING_TTL: Duration = Duration::from_secs(600);
const PENDING_MAX: usize = 32;

static PENDING: Mutex<Option<HashMap<String, (Secret, Instant)>>> = Mutex::new(None);

fn stash_secret(secret: Secret) -> String {
    let id = Uuid::new_v4().to_string();
    let mut guard = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.retain(|_, (_, at)| at.elapsed() < PENDING_TTL);
    if map.len() >= PENDING_MAX {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(id.clone(), (secret, Instant::now()));
    id
}

/// Take (and forget) the password parsed from a URI. `None` when unknown or expired.
///
/// Not called yet: `connection_save` / `session_open` must accept `pendingSecretId` (contract
/// change proposed in the package K report).
#[allow(dead_code)]
pub fn take_pending_secret(id: &str) -> Option<Secret> {
    let mut guard = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let (secret, at) = guard.as_mut()?.remove(id)?;
    (at.elapsed() < PENDING_TTL).then_some(secret)
}

fn invalid(msg: &str) -> Error {
    // Never echo the URI: it may contain the password.
    Error::Query {
        message: format!("invalid connection URI: {msg}"),
        position: None,
    }
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
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
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
            .map(|(h, port)| format!("{h}:{}", port.unwrap_or(27017)))
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
    Ok(cfg)
}

/// Parse a URI into a profile and the password (if any).
pub fn parse_uri(uri: &str) -> Result<(ConnectionProfile, Option<Secret>)> {
    let parts = split_uri(uri)?;
    let cfg = match parts.scheme.as_str() {
        "mongodb" | "mongodb+srv" => parse_mongo(&parts)?,
        "redis" | "rediss" => parse_redis(&parts)?,
        "postgres" | "postgresql" | "mysql" | "mariadb" | "sqlserver" | "mssql" | "oracle" => {
            parse_sql(&parts)?
        }
        other => return Err(Error::Unsupported(format!("uri_scheme:{other}"))),
    };
    Ok((profile_of(cfg), parts.password.map(Secret::new)))
}

pub(crate) fn parse_uri_impl(state: &AppState, uri: &str) -> Result<ParsedConnectionUri> {
    let _ = state;
    let (profile, secret) = parse_uri(uri)?;
    let has_password = secret.is_some();
    let pending_secret_id = secret.map(stash_secret);
    Ok(ParsedConnectionUri {
        profile,
        has_password,
        pending_secret_id,
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
        assert_eq!(s.unwrap().expose(), "p@ss/w:rd");
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
        assert!(s.is_none());
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
        assert_eq!(s.unwrap().expose(), "secret");
        assert_eq!(p.config.tls.mode, TlsMode::VerifyFull);
        assert!(parse_uri("redis://h/notanumber").is_err());

        let (p, s) =
            parse_uri("postgres://me:pw@db.local/shop?sslmode=verify-full&sslrootcert=/r.pem")
                .unwrap();
        assert_eq!((p.config.kind, p.config.port), (DbKind::Postgres, 5432));
        assert_eq!(p.config.tls.mode, TlsMode::VerifyFull);
        assert_eq!(p.config.tls.ca_cert_path.as_deref(), Some("/r.pem"));
        assert_eq!(s.unwrap().expose(), "pw");
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
        assert_eq!(parse_uri("not a uri").unwrap_err().kind(), "query");
        assert_eq!(parse_uri("mongodb://").unwrap_err().kind(), "query");
        assert!(parse_uri("mongodb://u:p%zz@h").is_err());
    }

    #[test]
    fn password_is_kept_only_in_the_backend() {
        let id = stash_secret(Secret::new("pw"));
        assert_eq!(take_pending_secret(&id).unwrap().expose(), "pw");
        assert!(take_pending_secret(&id).is_none(), "single use");
        let json = serde_json::to_string(&ParsedConnectionUri {
            profile: parse_uri("mongodb://u:pw@h/x").unwrap().0,
            has_password: true,
            pending_secret_id: Some("id".into()),
        })
        .unwrap();
        assert!(!json.contains("\"pw\""));
    }
}
