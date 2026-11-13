//! redis-cli line handling: tokenizer, command table and classification.

use dbstudio_core::{DestructiveReason, Error, Result, StatementClass, TxControl};

/// Command flags (bit set in [`CommandSpec::flags`]).
pub mod flags {
    /// Reads data.
    pub const READ: u8 = 1;
    /// Writes data.
    pub const WRITE: u8 = 2;
    /// Administrative.
    pub const ADMIN: u8 = 4;
    /// Can destroy data or hurt the server.
    pub const DANGEROUS: u8 = 8;
    /// May block the connection.
    pub const BLOCKING: u8 = 16;
    /// Pub/sub.
    pub const PUBSUB: u8 = 32;
}

/// Static description of a Redis command (from the Redis 7.4 docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// Upper-case name, e.g. `FLUSHDB` (`CONFIG SET` for sub-commands).
    pub name: &'static str,
    /// Arity as in `COMMAND INFO` (negative = at least).
    pub arity: i32,
    /// Bit set of [`flags`].
    pub flags: u8,
    /// One-line summary.
    pub summary: &'static str,
}

const R: u8 = flags::READ;
const W: u8 = flags::WRITE;
const A: u8 = flags::ADMIN;
const D: u8 = flags::DANGEROUS;
const B: u8 = flags::BLOCKING;
const P: u8 = flags::PUBSUB;

macro_rules! commands {
    ($( ($name:expr, $arity:expr, $flags:expr, $summary:expr, $syntax:expr) ),* $(,)?) => {
        /// Known commands.
        pub static COMMANDS: &[CommandSpec] = &[
            $( CommandSpec { name: $name, arity: $arity, flags: $flags, summary: $summary } ),*
        ];
        /// Syntax line of each entry of [`COMMANDS`] (same order).
        static SYNTAXES: &[&str] = &[ $( $syntax ),* ];
    };
}

commands! {
    // --- strings
    ("APPEND", 3, W, "Append a value to a key", "APPEND key value"),
    ("DECR", 2, W, "Decrement the integer value of a key by one", "DECR key"),
    ("DECRBY", 3, W, "Decrement the integer value of a key by a number", "DECRBY key decrement"),
    ("GET", 2, R, "Get the value of a key", "GET key"),
    ("GETDEL", 2, W, "Get the value of a key and delete it", "GETDEL key"),
    ("GETEX", -2, W, "Get the value of a key and set its expiration", "GETEX key [EX seconds | PX milliseconds | EXAT unix-time-seconds | PXAT unix-time-milliseconds | PERSIST]"),
    ("GETRANGE", 4, R, "Get a substring of the string stored at a key", "GETRANGE key start end"),
    ("GETSET", 3, W, "Set the string value of a key and return its old value", "GETSET key value"),
    ("INCR", 2, W, "Increment the integer value of a key by one", "INCR key"),
    ("INCRBY", 3, W, "Increment the integer value of a key by a number", "INCRBY key increment"),
    ("INCRBYFLOAT", 3, W, "Increment the float value of a key by a number", "INCRBYFLOAT key increment"),
    ("LCS", -3, R, "Find the longest common subsequence", "LCS key1 key2 [LEN] [IDX] [MINMATCHLEN len] [WITHMATCHLEN]"),
    ("MGET", -2, R, "Get the values of all the given keys", "MGET key [key ...]"),
    ("MSET", -3, W, "Set multiple keys to multiple values", "MSET key value [key value ...]"),
    ("MSETNX", -3, W, "Set multiple keys only if none of them exist", "MSETNX key value [key value ...]"),
    ("PSETEX", 4, W, "Set the value and expiration in milliseconds of a key", "PSETEX key milliseconds value"),
    ("SET", -3, W, "Set the string value of a key", "SET key value [NX | XX] [GET] [EX seconds | PX milliseconds | EXAT unix-time-seconds | PXAT unix-time-milliseconds | KEEPTTL]"),
    ("SETEX", 4, W, "Set the value and expiration of a key", "SETEX key seconds value"),
    ("SETNX", 3, W, "Set the value of a key, only if the key does not exist", "SETNX key value"),
    ("SETRANGE", 4, W, "Overwrite part of a string at key starting at the specified offset", "SETRANGE key offset value"),
    ("STRLEN", 2, R, "Get the length of the value stored in a key", "STRLEN key"),
    ("SUBSTR", 4, R, "Get a substring of the string stored at a key", "SUBSTR key start end"),
    // --- bitmaps / hyperloglog
    ("BITCOUNT", -2, R, "Count set bits in a string", "BITCOUNT key [start end [BYTE | BIT]]"),
    ("BITFIELD", -2, W, "Perform arbitrary bitfield integer operations on strings", "BITFIELD key [GET encoding offset | SET encoding offset value | INCRBY encoding offset increment | OVERFLOW WRAP | SAT | FAIL ...]"),
    ("BITFIELD_RO", -2, R, "Perform read-only bitfield integer operations", "BITFIELD_RO key [GET encoding offset ...]"),
    ("BITOP", -4, W, "Perform bitwise operations between strings", "BITOP AND | OR | XOR | NOT destkey key [key ...]"),
    ("BITPOS", -3, R, "Find first bit set or clear in a string", "BITPOS key bit [start [end [BYTE | BIT]]]"),
    ("GETBIT", 3, R, "Return the bit value at offset in the string value stored at key", "GETBIT key offset"),
    ("SETBIT", 4, W, "Set or clear the bit at offset in the string value stored at key", "SETBIT key offset value"),
    ("PFADD", -2, W, "Add the specified elements to the specified HyperLogLog", "PFADD key [element [element ...]]"),
    ("PFCOUNT", -2, R, "Return the approximated cardinality of the set(s) observed by the HyperLogLog", "PFCOUNT key [key ...]"),
    ("PFMERGE", -2, W, "Merge N different HyperLogLogs into a single one", "PFMERGE destkey [sourcekey [sourcekey ...]]"),
    // --- keys
    ("COPY", -3, W, "Copy a key", "COPY source destination [DB destination-db] [REPLACE]"),
    ("DEL", -2, W, "Delete a key", "DEL key [key ...]"),
    ("DUMP", 2, R, "Return a serialized version of the value stored at the specified key", "DUMP key"),
    ("EXISTS", -2, R, "Determine if a key exists", "EXISTS key [key ...]"),
    ("EXPIRE", -3, W, "Set a key's time to live in seconds", "EXPIRE key seconds [NX | XX | GT | LT]"),
    ("EXPIREAT", -3, W, "Set the expiration for a key as a UNIX timestamp", "EXPIREAT key unix-time-seconds [NX | XX | GT | LT]"),
    ("EXPIRETIME", 2, R, "Get the expiration Unix timestamp for a key", "EXPIRETIME key"),
    ("KEYS", 2, R | D, "Find all keys matching the given pattern", "KEYS pattern"),
    ("MIGRATE", -6, W | D, "Atomically transfer a key from a Redis instance to another one", "MIGRATE host port key | \"\" destination-db timeout [COPY] [REPLACE] [AUTH password | AUTH2 username password] [KEYS key [key ...]]"),
    ("MOVE", 3, W, "Move a key to another database", "MOVE key db"),
    ("PERSIST", 2, W, "Remove the expiration from a key", "PERSIST key"),
    ("PEXPIRE", -3, W, "Set a key's time to live in milliseconds", "PEXPIRE key milliseconds [NX | XX | GT | LT]"),
    ("PEXPIREAT", -3, W, "Set the expiration for a key as a UNIX timestamp specified in milliseconds", "PEXPIREAT key unix-time-milliseconds [NX | XX | GT | LT]"),
    ("PEXPIRETIME", 2, R, "Get the expiration Unix timestamp for a key in milliseconds", "PEXPIRETIME key"),
    ("PTTL", 2, R, "Get the time to live for a key in milliseconds", "PTTL key"),
    ("RANDOMKEY", 1, R, "Return a random key from the keyspace", "RANDOMKEY"),
    ("RENAME", 3, W, "Rename a key", "RENAME key newkey"),
    ("RENAMENX", 3, W, "Rename a key, only if the new key does not exist", "RENAMENX key newkey"),
    ("RESTORE", -4, W, "Create a key using the provided serialized value", "RESTORE key ttl serialized-value [REPLACE] [ABSTTL] [IDLETIME seconds] [FREQ frequency]"),
    ("SCAN", -2, R, "Incrementally iterate the keys space", "SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]"),
    ("SORT", -2, W, "Sort the elements in a list, set or sorted set", "SORT key [BY pattern] [LIMIT offset count] [GET pattern [GET pattern ...]] [ASC | DESC] [ALPHA] [STORE destination]"),
    ("SORT_RO", -2, R, "Sort the elements in a list, set or sorted set (read-only)", "SORT_RO key [BY pattern] [LIMIT offset count] [GET pattern ...] [ASC | DESC] [ALPHA]"),
    ("TOUCH", -2, R, "Alter the last access time of a key(s)", "TOUCH key [key ...]"),
    ("TTL", 2, R, "Get the time to live for a key in seconds", "TTL key"),
    ("TYPE", 2, R, "Determine the type stored at key", "TYPE key"),
    ("UNLINK", -2, W, "Delete a key asynchronously in another thread", "UNLINK key [key ...]"),
    ("WAIT", 3, A, "Wait for the synchronous replication of all the write commands", "WAIT numreplicas timeout"),
    ("OBJECT ENCODING", 3, R, "Inspect the internal encoding of a key", "OBJECT ENCODING key"),
    ("OBJECT FREQ", 3, R, "Get the logarithmic access frequency counter of a key", "OBJECT FREQ key"),
    ("OBJECT IDLETIME", 3, R, "Get the time since a key was last accessed", "OBJECT IDLETIME key"),
    ("OBJECT REFCOUNT", 3, R, "Get the number of references to the value of a key", "OBJECT REFCOUNT key"),
    ("OBJECT HELP", 2, R, "Show helpful text about the different subcommands", "OBJECT HELP"),
    // --- hashes
    ("HDEL", -3, W, "Delete one or more hash fields", "HDEL key field [field ...]"),
    ("HEXISTS", 3, R, "Determine if a hash field exists", "HEXISTS key field"),
    ("HGET", 3, R, "Get the value of a hash field", "HGET key field"),
    ("HGETALL", 2, R, "Get all the fields and values in a hash", "HGETALL key"),
    ("HINCRBY", 4, W, "Increment the integer value of a hash field by the given number", "HINCRBY key field increment"),
    ("HINCRBYFLOAT", 4, W, "Increment the float value of a hash field by the given amount", "HINCRBYFLOAT key field increment"),
    ("HKEYS", 2, R, "Get all the fields in a hash", "HKEYS key"),
    ("HLEN", 2, R, "Get the number of fields in a hash", "HLEN key"),
    ("HMGET", -3, R, "Get the values of all the given hash fields", "HMGET key field [field ...]"),
    ("HMSET", -4, W, "Set multiple hash fields to multiple values", "HMSET key field value [field value ...]"),
    ("HRANDFIELD", -2, R, "Get one or multiple random fields from a hash", "HRANDFIELD key [count [WITHVALUES]]"),
    ("HSCAN", -3, R, "Incrementally iterate hash fields and associated values", "HSCAN key cursor [MATCH pattern] [COUNT count] [NOVALUES]"),
    ("HSET", -4, W, "Set the string value of a hash field", "HSET key field value [field value ...]"),
    ("HSETNX", 4, W, "Set the value of a hash field, only if the field does not exist", "HSETNX key field value"),
    ("HSTRLEN", 3, R, "Get the length of the value of a hash field", "HSTRLEN key field"),
    ("HVALS", 2, R, "Get all the values in a hash", "HVALS key"),
    ("HEXPIRE", -6, W, "Set expiry for hash field using relative time to expire (seconds)", "HEXPIRE key seconds [NX | XX | GT | LT] FIELDS numfields field [field ...]"),
    ("HTTL", -5, R, "Returns the TTL in seconds of a hash field", "HTTL key FIELDS numfields field [field ...]"),
    // --- lists
    ("BLMOVE", 6, W | B, "Pop an element from a list, push it to another list and return it; or block until one is available", "BLMOVE source destination LEFT | RIGHT LEFT | RIGHT timeout"),
    ("BLMPOP", -5, W | B, "Pop elements from a list, or block until one is available", "BLMPOP timeout numkeys key [key ...] LEFT | RIGHT [COUNT count]"),
    ("BLPOP", -3, W | B, "Remove and get the first element in a list, or block until one is available", "BLPOP key [key ...] timeout"),
    ("BRPOP", -3, W | B, "Remove and get the last element in a list, or block until one is available", "BRPOP key [key ...] timeout"),
    ("BRPOPLPUSH", 4, W | B, "Pop an element from a list, push it to another list and return it; or block until one is available", "BRPOPLPUSH source destination timeout"),
    ("LINDEX", 3, R, "Get an element from a list by its index", "LINDEX key index"),
    ("LINSERT", 5, W, "Insert an element before or after another element in a list", "LINSERT key BEFORE | AFTER pivot element"),
    ("LLEN", 2, R, "Get the length of a list", "LLEN key"),
    ("LMOVE", 5, W, "Pop an element from a list, push it to another list and return it", "LMOVE source destination LEFT | RIGHT LEFT | RIGHT"),
    ("LMPOP", -4, W, "Pop elements from a list", "LMPOP numkeys key [key ...] LEFT | RIGHT [COUNT count]"),
    ("LPOP", -2, W, "Remove and get the first elements in a list", "LPOP key [count]"),
    ("LPOS", -3, R, "Return the index of matching elements on a list", "LPOS key element [RANK rank] [COUNT num-matches] [MAXLEN len]"),
    ("LPUSH", -3, W, "Prepend one or multiple elements to a list", "LPUSH key element [element ...]"),
    ("LPUSHX", -3, W, "Prepend an element to a list, only if the list exists", "LPUSHX key element [element ...]"),
    ("LRANGE", 4, R, "Get a range of elements from a list", "LRANGE key start stop"),
    ("LREM", 4, W, "Remove elements from a list", "LREM key count element"),
    ("LSET", 4, W, "Set the value of an element in a list by its index", "LSET key index element"),
    ("LTRIM", 4, W, "Trim a list to the specified range", "LTRIM key start stop"),
    ("RPOP", -2, W, "Remove and get the last elements in a list", "RPOP key [count]"),
    ("RPOPLPUSH", 3, W, "Remove the last element in a list, prepend it to another list and return it", "RPOPLPUSH source destination"),
    ("RPUSH", -3, W, "Append one or multiple elements to a list", "RPUSH key element [element ...]"),
    ("RPUSHX", -3, W, "Append an element to a list, only if the list exists", "RPUSHX key element [element ...]"),
    // --- sets
    ("SADD", -3, W, "Add one or more members to a set", "SADD key member [member ...]"),
    ("SCARD", 2, R, "Get the number of members in a set", "SCARD key"),
    ("SDIFF", -2, R, "Subtract multiple sets", "SDIFF key [key ...]"),
    ("SDIFFSTORE", -3, W, "Subtract multiple sets and store the resulting set in a key", "SDIFFSTORE destination key [key ...]"),
    ("SINTER", -2, R, "Intersect multiple sets", "SINTER key [key ...]"),
    ("SINTERCARD", -3, R, "Intersect multiple sets and return the cardinality of the result", "SINTERCARD numkeys key [key ...] [LIMIT limit]"),
    ("SINTERSTORE", -3, W, "Intersect multiple sets and store the resulting set in a key", "SINTERSTORE destination key [key ...]"),
    ("SISMEMBER", 3, R, "Determine if a given value is a member of a set", "SISMEMBER key member"),
    ("SMEMBERS", 2, R, "Get all the members in a set", "SMEMBERS key"),
    ("SMISMEMBER", -3, R, "Returns the membership associated with the given elements for a set", "SMISMEMBER key member [member ...]"),
    ("SMOVE", 4, W, "Move a member from one set to another", "SMOVE source destination member"),
    ("SPOP", -2, W, "Remove and return one or multiple random members from a set", "SPOP key [count]"),
    ("SRANDMEMBER", -2, R, "Get one or multiple random members from a set", "SRANDMEMBER key [count]"),
    ("SREM", -3, W, "Remove one or more members from a set", "SREM key member [member ...]"),
    ("SSCAN", -3, R, "Incrementally iterate Set elements", "SSCAN key cursor [MATCH pattern] [COUNT count]"),
    ("SUNION", -2, R, "Add multiple sets", "SUNION key [key ...]"),
    ("SUNIONSTORE", -3, W, "Add multiple sets and store the resulting set in a key", "SUNIONSTORE destination key [key ...]"),
    // --- sorted sets
    ("BZMPOP", -5, W | B, "Remove and return members with scores in a sorted set or block until one is available", "BZMPOP timeout numkeys key [key ...] MIN | MAX [COUNT count]"),
    ("BZPOPMAX", -3, W | B, "Remove and return the member with the highest score from one or more sorted sets, or block until one is available", "BZPOPMAX key [key ...] timeout"),
    ("BZPOPMIN", -3, W | B, "Remove and return the member with the lowest score from one or more sorted sets, or block until one is available", "BZPOPMIN key [key ...] timeout"),
    ("ZADD", -4, W, "Add one or more members to a sorted set, or update its score if it already exists", "ZADD key [NX | XX] [GT | LT] [CH] [INCR] score member [score member ...]"),
    ("ZCARD", 2, R, "Get the number of members in a sorted set", "ZCARD key"),
    ("ZCOUNT", 4, R, "Count the members in a sorted set with scores within the given values", "ZCOUNT key min max"),
    ("ZDIFF", -3, R, "Subtract multiple sorted sets", "ZDIFF numkeys key [key ...] [WITHSCORES]"),
    ("ZDIFFSTORE", -4, W, "Subtract multiple sorted sets and store the resulting sorted set in a new key", "ZDIFFSTORE destination numkeys key [key ...]"),
    ("ZINCRBY", 4, W, "Increment the score of a member in a sorted set", "ZINCRBY key increment member"),
    ("ZINTER", -3, R, "Intersect multiple sorted sets", "ZINTER numkeys key [key ...] [WEIGHTS weight ...] [AGGREGATE SUM | MIN | MAX] [WITHSCORES]"),
    ("ZINTERCARD", -3, R, "Intersect multiple sorted sets and return the cardinality of the result", "ZINTERCARD numkeys key [key ...] [LIMIT limit]"),
    ("ZINTERSTORE", -4, W, "Intersect multiple sorted sets and store the resulting sorted set in a new key", "ZINTERSTORE destination numkeys key [key ...] [WEIGHTS weight ...] [AGGREGATE SUM | MIN | MAX]"),
    ("ZLEXCOUNT", 4, R, "Count the number of members in a sorted set between a given lexicographical range", "ZLEXCOUNT key min max"),
    ("ZMPOP", -4, W, "Remove and return members with scores in a sorted set", "ZMPOP numkeys key [key ...] MIN | MAX [COUNT count]"),
    ("ZMSCORE", -3, R, "Get the score associated with the given members in a sorted set", "ZMSCORE key member [member ...]"),
    ("ZPOPMAX", -2, W, "Remove and return members with the highest scores in a sorted set", "ZPOPMAX key [count]"),
    ("ZPOPMIN", -2, W, "Remove and return members with the lowest scores in a sorted set", "ZPOPMIN key [count]"),
    ("ZRANDMEMBER", -2, R, "Get one or multiple random elements from a sorted set", "ZRANDMEMBER key [count [WITHSCORES]]"),
    ("ZRANGE", -4, R, "Return a range of members in a sorted set", "ZRANGE key start stop [BYSCORE | BYLEX] [REV] [LIMIT offset count] [WITHSCORES]"),
    ("ZRANGEBYLEX", -4, R, "Return a range of members in a sorted set, by lexicographical range", "ZRANGEBYLEX key min max [LIMIT offset count]"),
    ("ZRANGEBYSCORE", -4, R, "Return a range of members in a sorted set, by score", "ZRANGEBYSCORE key min max [WITHSCORES] [LIMIT offset count]"),
    ("ZRANGESTORE", -5, W, "Store a range of members from sorted set into another key", "ZRANGESTORE dst src min max [BYSCORE | BYLEX] [REV] [LIMIT offset count]"),
    ("ZRANK", -3, R, "Determine the index of a member in a sorted set", "ZRANK key member [WITHSCORE]"),
    ("ZREM", -3, W, "Remove one or more members from a sorted set", "ZREM key member [member ...]"),
    ("ZREMRANGEBYLEX", 4, W, "Remove all members in a sorted set between the given lexicographical range", "ZREMRANGEBYLEX key min max"),
    ("ZREMRANGEBYRANK", 4, W, "Remove all members in a sorted set within the given indexes", "ZREMRANGEBYRANK key start stop"),
    ("ZREMRANGEBYSCORE", 4, W, "Remove all members in a sorted set within the given scores", "ZREMRANGEBYSCORE key min max"),
    ("ZREVRANGE", -4, R, "Return a range of members in a sorted set, by index, with scores ordered from high to low", "ZREVRANGE key start stop [WITHSCORES]"),
    ("ZREVRANGEBYLEX", -4, R, "Return a range of members in a sorted set, by lexicographical range, ordered from higher to lower strings", "ZREVRANGEBYLEX key max min [LIMIT offset count]"),
    ("ZREVRANGEBYSCORE", -4, R, "Return a range of members in a sorted set, by score, with scores ordered from high to low", "ZREVRANGEBYSCORE key max min [WITHSCORES] [LIMIT offset count]"),
    ("ZREVRANK", -3, R, "Determine the index of a member in a sorted set, with scores ordered from high to low", "ZREVRANK key member [WITHSCORE]"),
    ("ZSCAN", -3, R, "Incrementally iterate sorted sets elements and associated scores", "ZSCAN key cursor [MATCH pattern] [COUNT count]"),
    ("ZSCORE", 3, R, "Get the score associated with the given member in a sorted set", "ZSCORE key member"),
    ("ZUNION", -3, R, "Add multiple sorted sets", "ZUNION numkeys key [key ...] [WEIGHTS weight ...] [AGGREGATE SUM | MIN | MAX] [WITHSCORES]"),
    ("ZUNIONSTORE", -4, W, "Add multiple sorted sets and store the resulting sorted set in a new key", "ZUNIONSTORE destination numkeys key [key ...] [WEIGHTS weight ...] [AGGREGATE SUM | MIN | MAX]"),
    // --- streams
    ("XACK", -4, W, "Marks a pending message as correctly processed", "XACK key group id [id ...]"),
    ("XADD", -5, W, "Appends a new message to a stream", "XADD key [NOMKSTREAM] [MAXLEN | MINID [= | ~] threshold [LIMIT count]] * | id field value [field value ...]"),
    ("XAUTOCLAIM", -6, W, "Changes, or acquires, ownership of messages in a consumer group", "XAUTOCLAIM key group consumer min-idle-time start [COUNT count] [JUSTID]"),
    ("XCLAIM", -6, W, "Changes, or acquires, ownership of a message in a consumer group", "XCLAIM key group consumer min-idle-time id [id ...] [IDLE ms] [TIME ms] [RETRYCOUNT count] [FORCE] [JUSTID]"),
    ("XDEL", -3, W, "Returns the number of messages after removing them from a stream", "XDEL key id [id ...]"),
    ("XGROUP CREATE", -5, W, "Create a consumer group", "XGROUP CREATE key group id | $ [MKSTREAM] [ENTRIESREAD entries-read]"),
    ("XGROUP CREATECONSUMER", 5, W, "Create a consumer in a consumer group", "XGROUP CREATECONSUMER key group consumer"),
    ("XGROUP DELCONSUMER", 5, W, "Delete a consumer from a consumer group", "XGROUP DELCONSUMER key group consumer"),
    ("XGROUP DESTROY", 4, W, "Destroy a consumer group", "XGROUP DESTROY key group"),
    ("XGROUP SETID", -5, W, "Set a consumer group to an arbitrary last delivered ID value", "XGROUP SETID key group id | $ [ENTRIESREAD entries-read]"),
    ("XINFO CONSUMERS", 4, R, "List the consumers in a consumer group", "XINFO CONSUMERS key group"),
    ("XINFO GROUPS", 3, R, "List the consumer groups of a stream", "XINFO GROUPS key"),
    ("XINFO STREAM", -3, R, "Get information about a stream", "XINFO STREAM key [FULL [COUNT count]]"),
    ("XLEN", 2, R, "Return the number of messages in a stream", "XLEN key"),
    ("XPENDING", -3, R, "Return information and entries from a stream consumer group pending entries list", "XPENDING key group [[IDLE min-idle-time] start end count [consumer]]"),
    ("XRANGE", -4, R, "Return a range of messages from a stream", "XRANGE key start end [COUNT count]"),
    ("XREAD", -4, R | B, "Return never seen elements in multiple streams", "XREAD [COUNT count] [BLOCK milliseconds] STREAMS key [key ...] id [id ...]"),
    ("XREADGROUP", -7, W | B, "Return new entries from a stream using a consumer group", "XREADGROUP GROUP group consumer [COUNT count] [BLOCK milliseconds] [NOACK] STREAMS key [key ...] id [id ...]"),
    ("XREVRANGE", -4, R, "Return a range of messages from a stream in reverse order", "XREVRANGE key end start [COUNT count]"),
    ("XSETID", -3, W, "An internal command for replicating stream values", "XSETID key last-id [ENTRIESADDED entries-added] [MAXDELETEDID max-deleted-id]"),
    ("XTRIM", -4, W, "Trims the stream to (approximately if '~' is passed) a certain size", "XTRIM key MAXLEN | MINID [= | ~] threshold [LIMIT count]"),
    // --- geo
    ("GEOADD", -5, W, "Add one or more geospatial items in the geospatial index represented using a sorted set", "GEOADD key [NX | XX] [CH] longitude latitude member [longitude latitude member ...]"),
    ("GEODIST", -4, R, "Returns the distance between two members of a geospatial index", "GEODIST key member1 member2 [M | KM | FT | MI]"),
    ("GEOHASH", -2, R, "Returns members of a geospatial index as standard geohash strings", "GEOHASH key [member [member ...]]"),
    ("GEOPOS", -2, R, "Returns longitude and latitude of members of a geospatial index", "GEOPOS key [member [member ...]]"),
    ("GEORADIUS", -6, W, "Query a sorted set representing a geospatial index to fetch members matching a given maximum distance from a point", "GEORADIUS key longitude latitude radius M | KM | FT | MI [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count [ANY]] [ASC | DESC] [STORE key | STOREDIST key]"),
    ("GEORADIUS_RO", -6, R, "Read-only variant of GEORADIUS", "GEORADIUS_RO key longitude latitude radius M | KM | FT | MI [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count [ANY]] [ASC | DESC]"),
    ("GEORADIUSBYMEMBER", -5, W, "Query a sorted set representing a geospatial index to fetch members matching a given maximum distance from a member", "GEORADIUSBYMEMBER key member radius M | KM | FT | MI [WITHCOORD] [WITHDIST] [WITHHASH] [COUNT count [ANY]] [ASC | DESC] [STORE key | STOREDIST key]"),
    ("GEOSEARCH", -7, R, "Query a sorted set representing a geospatial index to fetch members inside an area of a box or a circle", "GEOSEARCH key FROMMEMBER member | FROMLONLAT longitude latitude BYRADIUS radius M | KM | FT | MI | BYBOX width height M | KM | FT | MI [ASC | DESC] [COUNT count [ANY]] [WITHCOORD] [WITHDIST] [WITHHASH]"),
    ("GEOSEARCHSTORE", -8, W, "Query a sorted set representing a geospatial index to fetch members inside an area of a box or a circle, and store the result in another key", "GEOSEARCHSTORE destination source FROMMEMBER member | FROMLONLAT longitude latitude BYRADIUS radius M | KM | FT | MI | BYBOX width height M | KM | FT | MI [ASC | DESC] [COUNT count [ANY]] [STOREDIST]"),
    // --- pub/sub
    ("PSUBSCRIBE", -2, P, "Listen for messages published to channels matching the given patterns", "PSUBSCRIBE pattern [pattern ...]"),
    ("PUBLISH", 3, W, "Post a message to a channel", "PUBLISH channel message"),
    ("PUBSUB CHANNELS", -2, R, "List active channels", "PUBSUB CHANNELS [pattern]"),
    ("PUBSUB NUMPAT", 2, R, "Get the count of unique patterns pattern subscriptions", "PUBSUB NUMPAT"),
    ("PUBSUB NUMSUB", -2, R, "Get the count of subscribers for channels", "PUBSUB NUMSUB [channel [channel ...]]"),
    ("PUNSUBSCRIBE", -1, P, "Stop listening for messages posted to channels matching the given patterns", "PUNSUBSCRIBE [pattern [pattern ...]]"),
    ("SPUBLISH", 3, W, "Post a message to a shard channel", "SPUBLISH shardchannel message"),
    ("SSUBSCRIBE", -2, P, "Listen for messages published to the given shard channels", "SSUBSCRIBE shardchannel [shardchannel ...]"),
    ("SUBSCRIBE", -2, P, "Listen for messages published to the given channels", "SUBSCRIBE channel [channel ...]"),
    ("SUNSUBSCRIBE", -1, P, "Stop listening for messages posted to the given shard channels", "SUNSUBSCRIBE [shardchannel [shardchannel ...]]"),
    ("UNSUBSCRIBE", -1, P, "Stop listening for messages posted to the given channels", "UNSUBSCRIBE [channel [channel ...]]"),
    // --- transactions
    ("DISCARD", 1, R, "Discard all commands issued after MULTI", "DISCARD"),
    ("EXEC", 1, W, "Execute all commands issued after MULTI", "EXEC"),
    ("MULTI", 1, R, "Mark the start of a transaction block", "MULTI"),
    ("UNWATCH", 1, R, "Forget about all watched keys", "UNWATCH"),
    ("WATCH", -2, R, "Watch the given keys to determine execution of the MULTI/EXEC block", "WATCH key [key ...]"),
    // --- scripting
    ("EVAL", -3, W, "Execute a Lua script server side", "EVAL script numkeys [key [key ...]] [arg [arg ...]]"),
    ("EVAL_RO", -3, R, "Execute a read-only Lua script server side", "EVAL_RO script numkeys [key [key ...]] [arg [arg ...]]"),
    ("EVALSHA", -3, W, "Execute a Lua script server side", "EVALSHA sha1 numkeys [key [key ...]] [arg [arg ...]]"),
    ("EVALSHA_RO", -3, R, "Execute a read-only Lua script server side", "EVALSHA_RO sha1 numkeys [key [key ...]] [arg [arg ...]]"),
    ("FCALL", -3, W, "Invoke a function", "FCALL function numkeys [key [key ...]] [arg [arg ...]]"),
    ("FCALL_RO", -3, R, "Invoke a read-only function", "FCALL_RO function numkeys [key [key ...]] [arg [arg ...]]"),
    ("FUNCTION DELETE", 3, A, "Delete a function library", "FUNCTION DELETE library-name"),
    ("FUNCTION DUMP", 2, A, "Dump all functions into a serialized binary payload", "FUNCTION DUMP"),
    ("FUNCTION FLUSH", -2, A, "Deleting all functions", "FUNCTION FLUSH [ASYNC | SYNC]"),
    ("FUNCTION KILL", 2, A, "Kill the function currently in execution", "FUNCTION KILL"),
    ("FUNCTION LIST", -2, A, "Return information about the functions and libraries", "FUNCTION LIST [LIBRARYNAME library-name-pattern] [WITHCODE]"),
    ("FUNCTION LOAD", -3, A, "Create a function library", "FUNCTION LOAD [REPLACE] function-code"),
    ("FUNCTION RESTORE", -3, A, "Restore all the functions on the given payload", "FUNCTION RESTORE serialized-value [FLUSH | APPEND | REPLACE]"),
    ("FUNCTION STATS", 2, A, "Return information about the function currently running", "FUNCTION STATS"),
    ("SCRIPT EXISTS", -3, A, "Check existence of scripts in the script cache", "SCRIPT EXISTS sha1 [sha1 ...]"),
    ("SCRIPT FLUSH", -2, A, "Remove all the scripts from the script cache", "SCRIPT FLUSH [ASYNC | SYNC]"),
    ("SCRIPT KILL", 2, A, "Kill the script currently in execution", "SCRIPT KILL"),
    ("SCRIPT LOAD", 3, A, "Load the specified Lua script into the script cache", "SCRIPT LOAD script"),
    // --- connection
    ("AUTH", -2, A, "Authenticate to the server", "AUTH [username] password"),
    ("CLIENT GETNAME", 2, A, "Get the current connection name", "CLIENT GETNAME"),
    ("CLIENT ID", 2, A, "Returns the client ID for the current connection", "CLIENT ID"),
    ("CLIENT INFO", 2, A, "Returns information about the current client connection", "CLIENT INFO"),
    ("CLIENT KILL", -3, A, "Terminate open connections", "CLIENT KILL ip:port | [ID client-id] [TYPE NORMAL | MASTER | SLAVE | REPLICA | PUBSUB] [ADDR ip:port] [SKIPME YES | NO]"),
    ("CLIENT LIST", -2, A, "Get the list of client connections", "CLIENT LIST [TYPE NORMAL | MASTER | REPLICA | PUBSUB] [ID client-id [client-id ...]]"),
    ("CLIENT NO-EVICT", 3, A, "Set client eviction mode for the current connection", "CLIENT NO-EVICT ON | OFF"),
    ("CLIENT PAUSE", -3, A, "Stop processing commands from clients for some time", "CLIENT PAUSE timeout [WRITE | ALL]"),
    ("CLIENT REPLY", 3, A, "Instruct the server whether to reply to commands", "CLIENT REPLY ON | OFF | SKIP"),
    ("CLIENT SETINFO", 4, A, "Set client or library name and version for the current connection", "CLIENT SETINFO LIB-NAME libname | LIB-VER libver"),
    ("CLIENT SETNAME", 3, A, "Set the current connection name", "CLIENT SETNAME connection-name"),
    ("CLIENT TRACKING", -3, A, "Enable or disable server assisted client side caching support", "CLIENT TRACKING ON | OFF [REDIRECT client-id] [PREFIX prefix ...] [BCAST] [OPTIN] [OPTOUT] [NOLOOP]"),
    ("CLIENT UNPAUSE", 2, A, "Resume processing of clients that were paused", "CLIENT UNPAUSE"),
    ("ECHO", 2, R, "Echo the given string", "ECHO message"),
    ("HELLO", -1, R, "Handshake with Redis", "HELLO [protover [AUTH username password] [SETNAME clientname]]"),
    ("PING", -1, R, "Ping the server", "PING [message]"),
    ("QUIT", -1, R, "Close the connection", "QUIT"),
    ("RESET", 1, R, "Reset the connection", "RESET"),
    ("SELECT", 2, R, "Change the selected database for the current connection", "SELECT index"),
    // --- server
    ("ACL CAT", -2, A, "List the ACL categories or the commands inside a category", "ACL CAT [categoryname]"),
    ("ACL DELUSER", -3, A, "Remove the specified ACL users and the associated rules", "ACL DELUSER username [username ...]"),
    ("ACL GENPASS", -2, A, "Generate a pseudorandom secure password to use for ACL users", "ACL GENPASS [bits]"),
    ("ACL GETUSER", 3, A, "Get the rules for a specific ACL user", "ACL GETUSER username"),
    ("ACL LIST", 2, A, "List the current ACL rules in ACL config file format", "ACL LIST"),
    ("ACL LOG", -2, A, "List latest events denied because of ACLs in place", "ACL LOG [count | RESET]"),
    ("ACL SETUSER", -3, A, "Modify or create the rules for a specific ACL user", "ACL SETUSER username [rule [rule ...]]"),
    ("ACL USERS", 2, A, "List the username of all the configured ACL rules", "ACL USERS"),
    ("ACL WHOAMI", 2, A, "Return the name of the user associated to the current connection", "ACL WHOAMI"),
    ("BGREWRITEAOF", 1, A, "Asynchronously rewrite the append-only file", "BGREWRITEAOF"),
    ("BGSAVE", -1, A, "Asynchronously save the dataset to disk", "BGSAVE [SCHEDULE]"),
    ("COMMAND", -1, R, "Get array of Redis command details", "COMMAND"),
    ("COMMAND COUNT", 2, R, "Get total number of Redis commands", "COMMAND COUNT"),
    ("COMMAND DOCS", -2, R, "Get array of specific Redis command documentation", "COMMAND DOCS [command-name [command-name ...]]"),
    ("COMMAND INFO", -2, R, "Get array of specific Redis command details, or all when no argument is given", "COMMAND INFO [command-name [command-name ...]]"),
    ("COMMAND LIST", -2, R, "Get an array of Redis command names", "COMMAND LIST [FILTERBY MODULE module-name | ACLCAT category | PATTERN pattern]"),
    ("CONFIG GET", -3, R | A, "Get the values of configuration parameters", "CONFIG GET parameter [parameter ...]"),
    ("CONFIG RESETSTAT", 2, A, "Reset the stats returned by INFO", "CONFIG RESETSTAT"),
    ("CONFIG REWRITE", 2, A, "Rewrite the configuration file with the in memory configuration", "CONFIG REWRITE"),
    ("CONFIG SET", -4, A, "Set configuration parameters to the given values", "CONFIG SET parameter value [parameter value ...]"),
    ("DBSIZE", 1, R, "Return the number of keys in the selected database", "DBSIZE"),
    ("DEBUG", -2, A, "A container for debugging commands", "DEBUG subcommand [arg [arg ...]]"),
    ("FAILOVER", -1, A, "Start a coordinated failover between this server and one of its replicas", "FAILOVER [TO host port [FORCE]] [ABORT] [TIMEOUT milliseconds]"),
    ("FLUSHALL", -1, W | A | D, "Remove all keys from all databases", "FLUSHALL [ASYNC | SYNC]"),
    ("FLUSHDB", -1, W | A | D, "Remove all keys from the current database", "FLUSHDB [ASYNC | SYNC]"),
    ("INFO", -1, R, "Get information and statistics about the server", "INFO [section [section ...]]"),
    ("LASTSAVE", 1, R, "Get the UNIX time stamp of the last successful save to disk", "LASTSAVE"),
    ("LATENCY DOCTOR", 2, A, "Return a human readable latency analysis report", "LATENCY DOCTOR"),
    ("LATENCY LATEST", 2, A, "Return the latest latency samples for all events", "LATENCY LATEST"),
    ("LATENCY RESET", -2, A, "Reset latency data for one or more events", "LATENCY RESET [event [event ...]]"),
    ("LOLWUT", -1, R, "Display some computer art and the Redis version", "LOLWUT [VERSION version]"),
    ("MEMORY DOCTOR", 2, R, "Outputs memory problems report", "MEMORY DOCTOR"),
    ("MEMORY STATS", 2, R, "Show memory usage details", "MEMORY STATS"),
    ("MEMORY USAGE", -3, R, "Estimate the memory usage of a key", "MEMORY USAGE key [SAMPLES count]"),
    ("MODULE LIST", 2, A, "List all modules loaded by the server", "MODULE LIST"),
    ("MODULE LOAD", -3, A, "Load a module", "MODULE LOAD path [arg [arg ...]]"),
    ("MODULE UNLOAD", 3, A, "Unload a module", "MODULE UNLOAD name"),
    ("MONITOR", 1, A, "Listen for all requests received by the server in real time", "MONITOR"),
    ("PSYNC", -3, A, "Internal command used for replication", "PSYNC replicationid offset"),
    ("PURGE", -1, A, "Purge memory", "PURGE"),
    ("REPLICAOF", 3, A, "Make the server a replica of another instance, or promote it as master", "REPLICAOF host port | NO ONE"),
    ("ROLE", 1, R, "Return the role of the instance in the context of replication", "ROLE"),
    ("SAVE", 1, A, "Synchronously save the dataset to disk", "SAVE"),
    ("SHUTDOWN", -1, A | D, "Synchronously save the dataset to disk and then shut down the server", "SHUTDOWN [NOSAVE | SAVE] [NOW] [FORCE] [ABORT]"),
    ("SLAVEOF", 3, A, "Make the server a replica of another instance, or promote it as master", "SLAVEOF host port | NO ONE"),
    ("SLOWLOG GET", -2, A, "Get the slow log's entries", "SLOWLOG GET [count]"),
    ("SLOWLOG LEN", 2, A, "Get the slow log's length", "SLOWLOG LEN"),
    ("SLOWLOG RESET", 2, A, "Clear all entries from the slow log", "SLOWLOG RESET"),
    ("SWAPDB", 3, W | D, "Swaps two Redis databases", "SWAPDB index1 index2"),
    ("SYNC", 1, A, "Internal command used for replication", "SYNC"),
    ("TIME", 1, R, "Return the current server time", "TIME"),
    // --- cluster (read-mostly)
    ("CLUSTER INFO", 2, R, "Provides info about Redis Cluster node state", "CLUSTER INFO"),
    ("CLUSTER KEYSLOT", 3, R, "Returns the hash slot of the specified key", "CLUSTER KEYSLOT key"),
    ("CLUSTER MYID", 2, R, "Returns the node's id", "CLUSTER MYID"),
    ("CLUSTER NODES", 2, R, "Get Cluster config for the node", "CLUSTER NODES"),
    ("CLUSTER SHARDS", 2, R, "Get array of cluster slots to node mappings", "CLUSTER SHARDS"),
    ("CLUSTER SLOTS", 2, R, "Get array of Cluster slot to node mappings", "CLUSTER SLOTS"),
    ("READONLY", 1, R, "Enables read queries for a connection to a Redis Cluster replica node", "READONLY"),
    ("READWRITE", 1, R, "Disables read queries for a connection to a Redis Cluster replica node", "READWRITE"),
}

/// Commands whose first word has sub-commands (`CONFIG GET`).
const CONTAINERS: &[&str] = &[
    "ACL", "CLIENT", "CLUSTER", "COMMAND", "CONFIG", "DEBUG", "FUNCTION", "LATENCY", "MEMORY",
    "MODULE", "OBJECT", "PUBSUB", "SCRIPT", "SLOWLOG", "XGROUP", "XINFO",
];

fn upper(t: &[u8]) -> String {
    String::from_utf8_lossy(t).to_ascii_uppercase()
}

/// Entry of [`COMMANDS`] for a tokenized line (sub-command first, then the bare name).
pub fn find_command(tokens: &[Vec<u8>]) -> Option<&'static CommandSpec> {
    let first = upper(tokens.first()?);
    if CONTAINERS.contains(&first.as_str()) {
        if let Some(sub) = tokens.get(1) {
            let full = format!("{first} {}", upper(sub));
            if let Some(c) = COMMANDS.iter().find(|c| c.name == full) {
                return Some(c);
            }
        }
    }
    COMMANDS.iter().find(|c| c.name == first)
}

/// Syntax line of a command (`SET key value [NX | XX] ...`).
pub fn syntax(name: &str) -> Option<&'static str> {
    COMMANDS
        .iter()
        .position(|c| c.name == name)
        .map(|i| SYNTAXES[i])
}

fn bad(msg: &str) -> Error {
    Error::query(msg.to_string())
}

/// Split one line into arguments like redis-cli (`"..."` with escapes, `'...'` verbatim apart
/// from `\'`).
pub fn tokenize_line(line: &str) -> Result<Vec<Vec<u8>>> {
    let b = line.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut out: Vec<Vec<u8>> = Vec::new();
    let is_space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    loop {
        while i < n && is_space(b[i]) {
            i += 1;
        }
        if i >= n {
            return Ok(out);
        }
        let mut cur: Vec<u8> = Vec::new();
        let mut in_dq = false;
        let mut in_sq = false;
        loop {
            if in_dq {
                if i >= n {
                    return Err(bad("unbalanced quotes in request"));
                }
                if b[i] == b'\\'
                    && i + 3 < n
                    && b[i + 1] == b'x'
                    && b[i + 2].is_ascii_hexdigit()
                    && b[i + 3].is_ascii_hexdigit()
                {
                    let h = std::str::from_utf8(&b[i + 2..i + 4]).unwrap_or("00");
                    cur.push(u8::from_str_radix(h, 16).unwrap_or(0));
                    i += 4;
                } else if b[i] == b'\\' && i + 1 < n {
                    i += 1;
                    cur.push(match b[i] {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'b' => 0x08,
                        b'a' => 0x07,
                        c => c,
                    });
                    i += 1;
                } else if b[i] == b'"' {
                    // The closing quote must be followed by a space or the end.
                    if i + 1 < n && !is_space(b[i + 1]) {
                        return Err(bad("unbalanced quotes in request"));
                    }
                    i += 1;
                    break;
                } else {
                    cur.push(b[i]);
                    i += 1;
                }
            } else if in_sq {
                if i >= n {
                    return Err(bad("unbalanced quotes in request"));
                }
                if b[i] == b'\\' && i + 1 < n && b[i + 1] == b'\'' {
                    cur.push(b'\'');
                    i += 2;
                } else if b[i] == b'\'' {
                    if i + 1 < n && !is_space(b[i + 1]) {
                        return Err(bad("unbalanced quotes in request"));
                    }
                    i += 1;
                    break;
                } else {
                    cur.push(b[i]);
                    i += 1;
                }
            } else {
                if i >= n || is_space(b[i]) {
                    break;
                }
                match b[i] {
                    b'"' => in_dq = true,
                    b'\'' => in_sq = true,
                    c => cur.push(c),
                }
                i += 1;
            }
        }
        out.push(cur);
    }
}

/// Byte ranges `(start, end)` of the arguments of `line` in the original text, using the same
/// quoting rules as [`tokenize_line`] (lenient: an unbalanced quote runs to the end).
pub fn token_spans(line: &str) -> Vec<(usize, usize)> {
    let b = line.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut out = Vec::new();
    while i < n {
        while i < n && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let start = i;
        let mut quote: Option<u8> = None;
        while i < n {
            let c = b[i];
            match quote {
                Some(q) => {
                    if c == b'\\' {
                        i += 1;
                    } else if c == q {
                        quote = None;
                    }
                }
                None => {
                    if c.is_ascii_whitespace() {
                        break;
                    }
                    if c == b'"' || c == b'\'' {
                        quote = Some(c);
                    }
                }
            }
            i += 1;
        }
        out.push((start, i.min(n)));
    }
    out
}

fn contains_ci(hay: &[u8], needle: &str) -> bool {
    let n = needle.as_bytes();
    hay.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Class and destructive reason of a command line (already tokenized).
pub fn classify(tokens: &[Vec<u8>]) -> (StatementClass, Option<DestructiveReason>) {
    use StatementClass as C;
    if tokens.is_empty() {
        return (C::Empty, None);
    }
    let Some(spec) = find_command(tokens) else {
        return (C::Unknown, None);
    };
    match spec.name {
        "FLUSHDB" => return (C::Write, Some(DestructiveReason::FlushDb)),
        "FLUSHALL" => return (C::Write, Some(DestructiveReason::FlushAll)),
        "MULTI" => {
            return (
                C::Transaction {
                    op: TxControl::Begin,
                },
                None,
            );
        }
        "EXEC" => {
            return (
                C::Transaction {
                    op: TxControl::Commit,
                },
                None,
            );
        }
        "DISCARD" => {
            return (
                C::Transaction {
                    op: TxControl::Rollback,
                },
                None,
            );
        }
        "WATCH" | "UNWATCH" | "SELECT" | "AUTH" | "HELLO" | "RESET" | "QUIT" | "READONLY"
        | "READWRITE" | "CLIENT SETNAME" | "CLIENT SETINFO" | "CLIENT REPLY"
        | "CLIENT NO-EVICT" | "CLIENT TRACKING" => return (C::Session, None),
        "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" | "FCALL" | "FCALL_RO" => {
            let reason = if tokens.iter().skip(1).any(|t| contains_ci(t, "flushall")) {
                Some(DestructiveReason::FlushAll)
            } else if tokens.iter().skip(1).any(|t| contains_ci(t, "flushdb")) {
                Some(DestructiveReason::FlushDb)
            } else {
                None
            };
            return (C::Procedural, reason);
        }
        _ => {}
    }
    let f = spec.flags;
    let class = if f & flags::WRITE != 0 {
        C::Write
    } else if f & flags::READ != 0 {
        C::Read
    } else if f & flags::ADMIN != 0 {
        C::Admin
    } else if f & flags::PUBSUB != 0 {
        C::Read
    } else {
        C::Unknown
    };
    (class, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementClass as C;

    fn tok(line: &str) -> Vec<String> {
        tokenize_line(line)
            .unwrap()
            .into_iter()
            .map(|t| String::from_utf8_lossy(&t).into_owned())
            .collect()
    }

    fn class(line: &str) -> (StatementClass, Option<DestructiveReason>) {
        classify(&tokenize_line(line).unwrap())
    }

    #[test]
    fn tokenizer_basics() {
        assert_eq!(tok("GET key"), ["GET", "key"]);
        assert_eq!(tok("  SET   a   b  "), ["SET", "a", "b"]);
        assert!(tok("").is_empty());
        assert!(tok("   \t ").is_empty());
        assert_eq!(tok("SET k \"hello world\""), ["SET", "k", "hello world"]);
        assert_eq!(tok("SET k 'hello world'"), ["SET", "k", "hello world"]);
        assert_eq!(tok("SET k \"\""), ["SET", "k", ""]);
    }

    #[test]
    fn tokenizer_escapes() {
        assert_eq!(
            tok(r#"SET k "a\nb\tc\"d\\e""#),
            ["SET", "k", "a\nb\tc\"d\\e"]
        );
        assert_eq!(
            tokenize_line(r#"SET k "\xff\x00A""#).unwrap()[2],
            vec![0xff, 0x00, b'A']
        );
        assert_eq!(tok(r"SET k 'it\'s \n'"), ["SET", "k", "it's \\n"]);
        assert_eq!(tok(r#"SET k "\q""#), ["SET", "k", "q"]);
    }

    #[test]
    fn tokenizer_unbalanced_quotes() {
        for l in ["SET k \"abc", "SET k 'abc", "SET k \"a\"b", "SET k 'a'b"] {
            let e = tokenize_line(l).unwrap_err();
            assert_eq!(e.kind(), "query", "{l}");
        }
    }

    #[test]
    fn tokenizer_utf8_passthrough() {
        assert_eq!(tok("SET k é😀"), ["SET", "k", "é😀"]);
        assert_eq!(tok("SET k \"é\""), ["SET", "k", "é"]);
    }

    #[test]
    fn spans_follow_quotes() {
        let l = "SET k \"a b\" 'c d'";
        let s = token_spans(l);
        assert_eq!(s.len(), 4);
        assert_eq!(&l[s[2].0..s[2].1], "\"a b\"");
        assert_eq!(&l[s[3].0..s[3].1], "'c d'");
    }

    #[test]
    fn table_is_sane() {
        assert!(COMMANDS.len() > 200);
        for c in COMMANDS {
            assert_eq!(c.name, c.name.to_ascii_uppercase());
            assert!(!c.summary.is_empty());
            assert!(c.arity != 0, "{}", c.name);
            let syn = syntax(c.name).unwrap();
            assert!(syn.starts_with(c.name), "{} / {syn}", c.name);
        }
        let mut names: Vec<_> = COMMANDS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let len = names.len();
        names.dedup();
        assert_eq!(names.len(), len, "duplicate command names");
    }

    #[test]
    fn lookup_handles_subcommands_and_case() {
        let t = tokenize_line("config get maxmemory").unwrap();
        assert_eq!(find_command(&t).unwrap().name, "CONFIG GET");
        let t = tokenize_line("get a").unwrap();
        assert_eq!(find_command(&t).unwrap().name, "GET");
        let t = tokenize_line("FROB a").unwrap();
        assert!(find_command(&t).is_none());
        assert_eq!(syntax("GET"), Some("GET key"));
        assert_eq!(syntax("NOPE"), None);
    }

    #[test]
    fn classify_basic() {
        assert_eq!(class("GET k"), (C::Read, None));
        assert_eq!(class("get k"), (C::Read, None));
        assert_eq!(class("SET k v"), (C::Write, None));
        assert_eq!(class("DEL k"), (C::Write, None));
        assert_eq!(class("HSET h f v"), (C::Write, None));
        assert_eq!(class("SCAN 0 MATCH a*"), (C::Read, None));
        assert_eq!(class("KEYS *"), (C::Read, None));
        assert_eq!(class("INFO"), (C::Read, None));
        assert_eq!(class("PING"), (C::Read, None));
        assert_eq!(class("PUBLISH c m"), (C::Write, None));
        assert_eq!(class("SUBSCRIBE c"), (C::Read, None));
        assert_eq!(class(""), (C::Empty, None));
        assert_eq!(class("FROBNICATE x"), (C::Unknown, None));
    }

    #[test]
    fn classify_destructive() {
        assert_eq!(
            class("FLUSHDB"),
            (C::Write, Some(DestructiveReason::FlushDb))
        );
        assert_eq!(
            class("flushdb async"),
            (C::Write, Some(DestructiveReason::FlushDb))
        );
        assert_eq!(
            class("FLUSHALL"),
            (C::Write, Some(DestructiveReason::FlushAll))
        );
        assert_eq!(
            class("EVAL \"redis.call('flushall')\" 0"),
            (C::Procedural, Some(DestructiveReason::FlushAll))
        );
        assert_eq!(
            class("EVAL \"redis.call('FLUSHDB')\" 0"),
            (C::Procedural, Some(DestructiveReason::FlushDb))
        );
        assert_eq!(class("EVAL \"return 1\" 0"), (C::Procedural, None));
        assert_eq!(class("DEL a b c"), (C::Write, None));
    }

    #[test]
    fn classify_session_tx_admin() {
        assert_eq!(class("SELECT 3"), (C::Session, None));
        assert_eq!(class("AUTH pw"), (C::Session, None));
        assert_eq!(class("CLIENT SETNAME x"), (C::Session, None));
        assert_eq!(class("WATCH k"), (C::Session, None));
        assert_eq!(
            class("MULTI"),
            (
                C::Transaction {
                    op: TxControl::Begin
                },
                None
            )
        );
        assert_eq!(
            class("EXEC"),
            (
                C::Transaction {
                    op: TxControl::Commit
                },
                None
            )
        );
        assert_eq!(
            class("DISCARD"),
            (
                C::Transaction {
                    op: TxControl::Rollback
                },
                None
            )
        );
        assert_eq!(class("CONFIG SET maxmemory 1"), (C::Admin, None));
        assert_eq!(class("CONFIG GET maxmemory"), (C::Read, None));
        assert_eq!(class("SHUTDOWN"), (C::Admin, None));
        assert_eq!(class("CLIENT KILL ID 1"), (C::Admin, None));
        assert_eq!(class("ACL SETUSER u on"), (C::Admin, None));
        assert_eq!(class("SCRIPT FLUSH"), (C::Admin, None));
        assert_eq!(class("SWAPDB 0 1"), (C::Write, None));
        assert_eq!(class("EVALSHA abc 0"), (C::Procedural, None));
    }
}
