//! Redis Cluster routing, kept small on purpose: the rest of the driver talks to plain
//! `MultiplexedConnection`s, and `Shared` picks the connection of the primary that owns the key
//! of each command (hash slot = CRC16(key or `{hash tag}`) mod 16384). `MOVED` / `ASK`
//! redirections refresh the slot map or retry on the named node. `SCAN` walks every primary in
//! turn behind one opaque cursor, so the key tree and the console work unchanged.

/// Number of hash slots of a Redis Cluster.
pub const SLOTS: u16 = 16384;

/// Bits of a composite `SCAN` cursor left for the cursor of one node; the node index sits above.
const NODE_SHIFT: u32 = 52;
const NODE_MASK: u64 = (1 << NODE_SHIFT) - 1;

/// CRC16-CCITT (XMODEM), the checksum Redis Cluster hashes keys with.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// The part of `key` that is hashed: the first non-empty `{...}` hash tag, else the whole key.
pub fn hash_tag(key: &[u8]) -> &[u8] {
    if let Some(open) = key.iter().position(|&b| b == b'{') {
        if let Some(len) = key[open + 1..].iter().position(|&b| b == b'}') {
            if len > 0 {
                return &key[open + 1..open + 1 + len];
            }
        }
    }
    key
}

/// Hash slot of `key`.
pub fn slot(key: &[u8]) -> u16 {
    crc16(hash_tag(key)) % SLOTS
}

/// Commands that name no key: they go to any primary (or are answered for the whole cluster).
const KEYLESS: &[&str] = &[
    "PING",
    "INFO",
    "DBSIZE",
    "COMMAND",
    "SCAN",
    "CLIENT",
    "CONFIG",
    "CLUSTER",
    "TIME",
    "ECHO",
    "HELLO",
    "AUTH",
    "SELECT",
    "FLUSHDB",
    "FLUSHALL",
    "RANDOMKEY",
    "LASTSAVE",
    "ROLE",
    "SLOWLOG",
    "LATENCY",
    "ACL",
    "MODULE",
    "FUNCTION",
    "SCRIPT",
    "PUBSUB",
    "QUIT",
    "RESET",
    "MULTI",
    "EXEC",
    "DISCARD",
    "UNWATCH",
    "WAIT",
    "KEYS",
];

/// Index of the argument holding the routing key of `args` (`args[0]` is the command name), or
/// `None` for a command without key.
pub fn key_index(args: &[Vec<u8>]) -> Option<usize> {
    let name = String::from_utf8_lossy(args.first()?).to_ascii_uppercase();
    if KEYLESS.contains(&name.as_str()) {
        return None;
    }
    let at = match name.as_str() {
        // `OBJECT ENCODING key`, `MEMORY USAGE key`, `XINFO STREAM key`.
        "OBJECT" | "MEMORY" | "XINFO" => 2,
        // `EVAL script numkeys key…`: no key when numkeys is 0.
        "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" | "FCALL" | "FCALL_RO" => {
            let n: usize = std::str::from_utf8(args.get(2)?).ok()?.parse().ok()?;
            if n == 0 {
                return None;
            }
            3
        }
        // `XREAD [COUNT n] [BLOCK ms] STREAMS key…`.
        "XREAD" | "XREADGROUP" => {
            let streams = args
                .iter()
                .position(|a| a.eq_ignore_ascii_case(b"STREAMS"))?;
            streams + 1
        }
        _ => 1,
    };
    (at < args.len()).then_some(at)
}

/// One primary of the slot map: the first and last slot it serves and its address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRange {
    pub first: u16,
    pub last: u16,
    pub node: String,
}

/// Parse a `CLUSTER SLOTS` reply. An empty host means "the node that answered" (`seed`).
pub fn parse_slots(reply: &redis::Value, seed: &str) -> Vec<SlotRange> {
    use redis::Value as V;
    let text = |v: &V| match v {
        V::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        V::SimpleString(s) => Some(s.clone()),
        _ => None,
    };
    let int = |v: &V| match v {
        V::Int(n) => Some(*n),
        _ => text(v).and_then(|t| t.parse().ok()),
    };
    let seed_host = seed.rsplit_once(':').map_or(seed, |(h, _)| h);
    let mut out = Vec::new();
    let V::Array(ranges) = reply else { return out };
    for r in ranges {
        let V::Array(parts) = r else { continue };
        let (Some(first), Some(last), Some(V::Array(primary))) = (
            parts.first().and_then(int),
            parts.get(1).and_then(int),
            parts.get(2),
        ) else {
            continue;
        };
        let host = primary.first().and_then(text).unwrap_or_default();
        let Some(port) = primary.get(1).and_then(int) else {
            continue;
        };
        let host = if host.is_empty() || host == "?" {
            seed_host.to_string()
        } else {
            host
        };
        out.push(SlotRange {
            first: first as u16,
            last: last as u16,
            node: node_addr(&host, port as u16),
        });
    }
    out.sort_by_key(|r| r.first);
    out
}

/// `host:port`, with brackets around an IPv6 host.
pub fn node_addr(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// `(host, port)` of a `host:port` / `[v6]:port` address.
pub fn split_addr(addr: &str) -> Option<(String, u16)> {
    let (host, port) = addr.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some((host.to_string(), port.parse().ok()?))
}

/// The primary serving `slot`.
pub fn node_for(ranges: &[SlotRange], slot: u16) -> Option<&str> {
    ranges
        .iter()
        .find(|r| r.first <= slot && slot <= r.last)
        .map(|r| r.node.as_str())
}

/// The distinct primaries of the slot map, in slot order.
pub fn primaries(ranges: &[SlotRange]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in ranges {
        if !out.contains(&r.node) {
            out.push(r.node.clone());
        }
    }
    out
}

/// A redirection in an error reply: `MOVED <slot> <host:port>` or `ASK <slot> <host:port>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    Moved(String),
    Ask(String),
}

pub fn redirect(code: Option<&str>, detail: Option<&str>) -> Option<Redirect> {
    let addr = detail?.split_whitespace().nth(1)?.to_string();
    match code? {
        "MOVED" => Some(Redirect::Moved(addr)),
        "ASK" => Some(Redirect::Ask(addr)),
        _ => None,
    }
}

/// One cursor for a `SCAN` over every primary: node index in the high bits, the cursor of that
/// node below. `0` starts at the first node and means "done" when returned.
pub fn encode_cursor(node: usize, cursor: u64) -> Option<u64> {
    (cursor <= NODE_MASK).then_some(((node as u64) << NODE_SHIFT) | cursor)
}

pub fn decode_cursor(cursor: u64) -> (usize, u64) {
    ((cursor >> NODE_SHIFT) as usize, cursor & NODE_MASK)
}

/// Next composite cursor after node `node` answered `next` (its own cursor), out of `nodes`.
pub fn next_cursor(node: usize, next: u64, nodes: usize) -> Option<u64> {
    if next != 0 {
        encode_cursor(node, next)
    } else if node + 1 < nodes {
        // Never 0 (that means "done"): the node index is at least 1 here.
        encode_cursor(node + 1, 0)
    } else {
        Some(0)
    }
}

/// Sum the `db0:keys=…,expires=…` lines of several `INFO keyspace` replies into one.
pub fn merge_keyspace(replies: &[String]) -> String {
    let (mut keys, mut expires) = (0u64, 0u64);
    for r in replies {
        for line in r.lines() {
            let Some(rest) = line.trim().strip_prefix("db0:") else {
                continue;
            };
            for kv in rest.split(',') {
                match kv.split_once('=') {
                    Some(("keys", v)) => keys += v.parse::<u64>().unwrap_or(0),
                    Some(("expires", v)) => expires += v.parse::<u64>().unwrap_or(0),
                    _ => {}
                }
            }
        }
    }
    if keys == 0 && expires == 0 {
        "# Keyspace\r\n".to_string()
    } else {
        format!("# Keyspace\r\ndb0:keys={keys},expires={expires},avg_ttl=0\r\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::Value as V;

    fn args(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn slots_match_redis() {
        // Values from the Redis Cluster specification and `CLUSTER KEYSLOT`.
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(slot(b"foo"), 12182);
        assert_eq!(slot(b"{user1000}.following"), slot(b"{user1000}.followers"));
        assert_eq!(slot(b"foo{}{bar}"), slot(b"foo{}{bar}")); // empty tag: whole key
        assert_eq!(hash_tag(b"foo{}{bar}"), b"foo{}{bar}");
        assert_eq!(hash_tag(b"foo{{bar}}zap"), b"{bar");
    }

    #[test]
    fn routing_keys_are_found() {
        assert_eq!(key_index(&args(&["GET", "k"])), Some(1));
        assert_eq!(key_index(&args(&["object", "encoding", "k"])), Some(2));
        assert_eq!(key_index(&args(&["EVAL", "return 1", "0"])), None);
        assert_eq!(key_index(&args(&["EVAL", "s", "1", "k"])), Some(3));
        assert_eq!(
            key_index(&args(&["XREAD", "COUNT", "2", "STREAMS", "s", "0"])),
            Some(4)
        );
        assert_eq!(key_index(&args(&["SCAN", "0"])), None);
        assert_eq!(key_index(&args(&["GET"])), None);
    }

    #[test]
    fn cluster_slots_are_parsed() {
        let node = |h: &str, p: i64| V::Array(vec![V::BulkString(h.into()), V::Int(p)]);
        let reply = V::Array(vec![
            V::Array(vec![V::Int(5461), V::Int(10922), node("10.0.0.2", 7001)]),
            V::Array(vec![V::Int(0), V::Int(5460), node("", 7000)]),
        ]);
        let ranges = parse_slots(&reply, "10.0.0.1:7000");
        assert_eq!(
            ranges[0],
            SlotRange {
                first: 0,
                last: 5460,
                node: "10.0.0.1:7000".into()
            }
        );
        assert_eq!(node_for(&ranges, 6000), Some("10.0.0.2:7001"));
        assert_eq!(node_for(&ranges, 16000), None);
        assert_eq!(primaries(&ranges).len(), 2);
        assert_eq!(split_addr("[::1]:7000"), Some(("::1".to_string(), 7000)));
    }

    #[test]
    fn redirections_and_cursors() {
        assert_eq!(
            redirect(Some("MOVED"), Some("3999 127.0.0.1:6381")),
            Some(Redirect::Moved("127.0.0.1:6381".into()))
        );
        assert_eq!(redirect(Some("ERR"), Some("x y")), None);
        // Node 0 still scanning, then node 1 starts, then done after the last node.
        let c = next_cursor(0, 17, 3).unwrap();
        assert_eq!(decode_cursor(c), (0, 17));
        let c = next_cursor(0, 0, 3).unwrap();
        assert_ne!(c, 0);
        assert_eq!(decode_cursor(c), (1, 0));
        assert_eq!(next_cursor(2, 0, 3), Some(0));
    }

    #[test]
    fn keyspaces_are_summed() {
        let merged = merge_keyspace(&[
            "# Keyspace\r\ndb0:keys=3,expires=1,avg_ttl=5\r\n".into(),
            "# Keyspace\r\ndb0:keys=4,expires=0,avg_ttl=0\r\n".into(),
        ]);
        assert!(merged.contains("db0:keys=7,expires=1"), "{merged}");
    }
}
