//! Editor hints: keywords, functions and (Redis/Mongo) commands per dialect.

use dbstudio_core::ScriptDialect;
use serde::{Deserialize, Serialize};

/// Completion hints for the editor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vocabulary {
    /// SQL keywords.
    pub keywords: Vec<String>,
    /// Built-in functions.
    pub functions: Vec<String>,
    /// Commands (Redis commands, Mongo methods).
    pub commands: Vec<CommandHint>,
}

/// A command with its syntax, for completion and signature help.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandHint {
    /// Command name.
    pub name: String,
    /// Syntax line, e.g. `SET key value [EX seconds]`.
    pub syntax: String,
    /// One-line summary.
    pub summary: String,
    /// Flags as text (`write`, `dangerous`...).
    pub flags: Vec<String>,
}

const BASE_KEYWORDS: &str = "SELECT FROM WHERE GROUP BY HAVING ORDER LIMIT OFFSET DISTINCT ALL AS ON JOIN INNER LEFT RIGHT FULL OUTER CROSS NATURAL USING UNION INTERSECT EXCEPT INSERT INTO VALUES UPDATE SET DELETE CREATE ALTER DROP TRUNCATE TABLE VIEW INDEX UNIQUE PRIMARY KEY FOREIGN REFERENCES CONSTRAINT CHECK DEFAULT NOT NULL AND OR IN IS LIKE BETWEEN EXISTS CASE WHEN THEN ELSE END ASC DESC WITH RECURSIVE BEGIN COMMIT ROLLBACK SAVEPOINT TRANSACTION GRANT REVOKE COLUMN ADD CASCADE RESTRICT IF TEMPORARY SCHEMA DATABASE SEQUENCE TRIGGER FUNCTION PROCEDURE RETURNS RETURN DECLARE FOR EACH ROW OVER PARTITION WINDOW ROWS RANGE UNBOUNDED PRECEDING FOLLOWING CURRENT CAST TRUE FALSE ANY SOME EXPLAIN ANALYZE";

const PG_KEYWORDS: &str = "RETURNING ILIKE SIMILAR LATERAL MATERIALIZED UNLOGGED VACUUM REINDEX CLUSTER COPY LISTEN NOTIFY UNLISTEN DO LANGUAGE PLPGSQL INHERITS TABLESPACE EXTENSION OWNER OWNED CONCURRENTLY ONLY NOWAIT SKIP LOCKED CONFLICT NOTHING ARRAY JSONB SERIAL BIGSERIAL TEXT BYTEA UUID INTERVAL TIMESTAMPTZ ENUM DOMAIN POLICY PUBLICATION SUBSCRIPTION GENERATED ALWAYS IDENTITY STORED FETCH NEXT ISOLATION LEVEL READ COMMITTED SERIALIZABLE REPEATABLE";

const MYSQL_KEYWORDS: &str = "SHOW DESCRIBE USE ENGINE INNODB AUTO_INCREMENT UNSIGNED ZEROFILL IGNORE REPLACE DUPLICATE KEYS STRAIGHT_JOIN DELIMITER PROCEDURE CALL LOCK UNLOCK TABLES FLUSH OPTIMIZE REPAIR CHARSET COLLATE CHARACTER ENUM TINYINT MEDIUMINT LONGTEXT MEDIUMTEXT LONGBLOB DATETIME TIMESTAMP DATABASES COLUMNS STATUS VARIABLES PROCESSLIST GLOBAL SESSION XA PARTITIONS EVENT TRIGGERS";

const MSSQL_KEYWORDS: &str = "GO TOP PRINT EXEC EXECUTE IDENTITY NOLOCK ROWLOCK READPAST TRY CATCH THROW RAISERROR WAITFOR OUTPUT INSERTED DELETED MERGE MATCHED SYNONYM CLUSTERED NONCLUSTERED NVARCHAR VARCHAR DATETIME2 DATETIMEOFFSET UNIQUEIDENTIFIER BIT MONEY NVARCHAR OPENQUERY OPENROWSET APPLY PIVOT UNPIVOT CROSS OUTER CHECKPOINT DBCC BACKUP RESTORE USE SCOPE_IDENTITY";

const ORACLE_KEYWORDS: &str = "DUAL ROWNUM ROWID CONNECT PRIOR START NOCYCLE MINUS MERGE MATCHED SEQUENCE NEXTVAL CURRVAL VARCHAR2 NVARCHAR2 NUMBER CLOB BLOB DATE TIMESTAMP PACKAGE BODY EXCEPTION WHEN LOOP WHILE EXIT CURSOR OPEN FETCH CLOSE BULK COLLECT FORALL PRAGMA AUTONOMOUS_TRANSACTION SYNONYM TABLESPACE PURGE FLASHBACK DBMS_OUTPUT PUT_LINE IMMEDIATE EXECUTE";

const BASE_FUNCTIONS: &str = "count sum avg min max coalesce nullif abs round ceil floor power sqrt mod length lower upper trim ltrim rtrim substring replace concat position cast row_number rank dense_rank lag lead first_value last_value ntile";

const PG_FUNCTIONS: &str = "now current_date current_timestamp date_trunc date_part extract age to_char to_date to_timestamp to_number string_agg array_agg json_agg jsonb_agg json_build_object jsonb_build_object jsonb_set jsonb_each jsonb_array_elements unnest generate_series regexp_replace regexp_matches split_part left right lpad rpad initcap md5 gen_random_uuid random setval nextval currval pg_sleep pg_size_pretty pg_total_relation_size pg_get_viewdef pg_terminate_backend pg_cancel_backend greatest least";

const MYSQL_FUNCTIONS: &str = "now curdate curtime date_format str_to_date date_add date_sub datediff timestampdiff timestampadd unix_timestamp from_unixtime ifnull if group_concat json_extract json_object json_array json_unquote concat_ws substring_index locate instr lpad rpad format uuid last_insert_id row_count found_rows database user version sleep greatest least rand";

const MSSQL_FUNCTIONS: &str = "getdate getutcdate sysdatetime dateadd datediff datepart datename format isnull iif len charindex patindex stuff string_agg string_split concat concat_ws cast convert try_cast try_convert newid scope_identity ident_current object_id db_name suser_sname openjson json_value json_query error_message error_number rand";

const ORACLE_FUNCTIONS: &str = "sysdate systimestamp current_date add_months months_between last_day next_day trunc to_char to_date to_number to_timestamp nvl nvl2 decode listagg regexp_like regexp_substr regexp_replace instr substr lpad rpad rownum sys_guid user userenv dbms_random.value json_value json_table greatest least";

const CQL_BASE_KEYWORDS: &str = "SELECT FROM WHERE GROUP BY ORDER LIMIT DISTINCT AS INSERT INTO VALUES UPDATE SET DELETE CREATE ALTER DROP TRUNCATE TABLE INDEX PRIMARY KEY AND OR IN IS NOT NULL LIKE ASC DESC BEGIN GRANT REVOKE ADD RENAME TO ON TRUE FALSE WITH IF EXISTS";

const CQL_KEYWORDS: &str = "KEYSPACE KEYSPACES TABLES TYPE TYPES MATERIALIZED VIEW USE CONSISTENCY SERIAL TRACING PAGING LIST ROLES ROLE USER USERS PERMISSIONS PERMISSION AUTHORIZE LOGIN SUPERUSER PASSWORD OPTIONS IF EXISTS BATCH UNLOGGED COUNTER APPLY USING TTL TIMESTAMP ALLOW FILTERING PER PARTITION CLUSTERING PRIMARY STATIC FROZEN WITH REPLICATION CLASS SIMPLESTRATEGY NETWORKTOPOLOGYSTRATEGY DURABLE_WRITES COMPACT STORAGE COMPACTION CACHING GC_GRACE_SECONDS BLOOM_FILTER_FP_CHANCE DEFAULT_TIME_TO_LIVE COMMENT TOKEN WRITETIME JSON DISTINCT CONTAINS ENTRIES FUNCTION FUNCTIONS AGGREGATE AGGREGATES CALLED RETURNS NULL INPUT LANGUAGE SFUNC STYPE FINALFUNC INITCOND CUSTOM DESCRIBE CLUSTER SCHEMA FULL INTERNALS ONLY INDEXES TRIGGER MASKED";

const CQL_FUNCTIONS: &str = "now uuid toTimestamp toDate toUnixTimestamp unixTimestampOf dateOf minTimeuuid maxTimeuuid currentTimestamp currentDate currentTime currentTimeUUID token ttl writetime count sum avg min max cast blobAsText textAsBlob blobAsBigint bigintAsBlob blobAsInt intAsBlob blobAsUuid uuidAsBlob blobAsTimestamp timestampAsBlob blobAsBoolean booleanAsBlob blobAsDouble doubleAsBlob blobAsFloat floatAsBlob blobAsInet inetAsBlob blobAsVarint varintAsBlob blobAsDecimal decimalAsBlob blobAsDate dateAsBlob blobAsTime timeAsBlob blobAsSmallint smallintAsBlob blobAsTinyint tinyintAsBlob similarity_cosine similarity_euclidean similarity_dot_product";

const CH_KEYWORDS: &str = "FORMAT SETTINGS FINAL SAMPLE PREWHERE ARRAY JOIN GLOBAL ANY ASOF SEMI ANTI LIMIT BY WITH TOTALS ROLLUP CUBE QUALIFY INTERVAL TTL ENGINE MERGETREE REPLACINGMERGETREE SUMMINGMERGETREE AGGREGATINGMERGETREE COLLAPSINGMERGETREE REPLICATEDMERGETREE DISTRIBUTED MEMORY LOG TINYLOG ORDER PARTITION PRIMARY SAMPLE CODEC TTL MATERIALIZED ALIAS EPHEMERAL LIVE WINDOW DICTIONARY DICTIONARIES DATABASES TABLES PROCESSLIST ATTACH DETACH PERMANENTLY EXCHANGE RENAME OPTIMIZE DEDUPLICATE SYSTEM FLUSH LOGS RELOAD SYNC REPLICA KILL QUERY MUTATION SYNC ASYNC TEST ON CLUSTER USE SET ROLE USER GRANT REVOKE IDENTIFIED BY HOST QUOTA POLICY PROFILE BACKUP RESTORE EXPLAIN AST SYNTAX PLAN PIPELINE ESTIMATE EXISTS DESCRIBE UNDROP MODIFY COLUMN COMMENT CLEAR INDEX PROJECTION FREEZE UNFREEZE FETCH MOVE TO VOLUME DISK UPDATE DELETE IN IF NOT NULL NULLS FIRST LAST COLLATE WATCH EVENTS INSERT INFILE OUTFILE COMPRESSION LEVEL";

const CH_FUNCTIONS: &str = "now today yesterday toDate toDateTime toDateTime64 toStartOfDay toStartOfHour toStartOfMonth toStartOfWeek toYYYYMM toYear toMonth toDayOfWeek formatDateTime parseDateTimeBestEffort dateDiff dateAdd dateSub toUnixTimestamp fromUnixTimestamp toString toInt32 toInt64 toUInt32 toUInt64 toFloat64 toDecimal64 toUUID generateUUIDv4 cityHash64 sipHash64 xxHash64 murmurHash3_64 MD5 SHA256 hex unhex base64Encode base64Decode count uniq uniqExact uniqCombined quantile quantiles median avg sum min max any anyLast argMin argMax groupArray groupUniqArray groupArrayMovingAvg topK sumIf countIf avgIf minIf maxIf sumMap arrayJoin arrayMap arrayFilter arrayExists arrayAll arrayReduce arraySum arraySort arrayReverse arrayDistinct arrayConcat arrayElement length empty notEmpty has hasAll hasAny indexOf range mapKeys mapValues tupleElement JSONExtract JSONExtractString JSONExtractInt JSONExtractFloat JSONExtractRaw JSONHas JSONLength lower upper lowerUTF8 upperUTF8 substring substringUTF8 concat splitByChar splitByString replaceOne replaceAll replaceRegexpAll extract match like position trim trimLeft trimRight format if multiIf coalesce ifNull nullIf assumeNotNull isNull isNotNull toTypeName currentDatabase hostName version uptime rand rand64 randomString numbers runningDifference neighbor rowNumberInBlock row_number rank dense_rank lagInFrame leadInFrame first_value last_value";

fn words(s: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    s.split_whitespace()
        .filter(|w| seen.insert(w.to_ascii_uppercase()))
        .map(str::to_string)
        .collect()
}

fn merged(parts: &[&str]) -> Vec<String> {
    words(&parts.join(" "))
}

fn flag_names(f: u8) -> Vec<String> {
    use crate::redis_cli::flags;
    let mut out = Vec::new();
    for (bit, name) in [
        (flags::READ, "read"),
        (flags::WRITE, "write"),
        (flags::ADMIN, "admin"),
        (flags::DANGEROUS, "dangerous"),
        (flags::BLOCKING, "blocking"),
        (flags::PUBSUB, "pubsub"),
    ] {
        if f & bit != 0 {
            out.push(name.to_string());
        }
    }
    out
}

const MONGO_METHODS: &[(&str, &str, &str, &[&str])] = &[
    (
        "find",
        "db.<coll>.find(filter, projection)",
        "Select documents in a collection",
        &["read"],
    ),
    (
        "findOne",
        "db.<coll>.findOne(filter, projection)",
        "Return one document that satisfies the filter",
        &["read"],
    ),
    (
        "aggregate",
        "db.<coll>.aggregate(pipeline, options)",
        "Run an aggregation pipeline",
        &["read"],
    ),
    (
        "countDocuments",
        "db.<coll>.countDocuments(filter)",
        "Count documents matching the filter",
        &["read"],
    ),
    (
        "estimatedDocumentCount",
        "db.<coll>.estimatedDocumentCount()",
        "Estimated number of documents from metadata",
        &["read"],
    ),
    (
        "distinct",
        "db.<coll>.distinct(field, filter)",
        "Distinct values of a field",
        &["read"],
    ),
    (
        "insertOne",
        "db.<coll>.insertOne(document)",
        "Insert a document",
        &["write"],
    ),
    (
        "insertMany",
        "db.<coll>.insertMany([documents])",
        "Insert several documents",
        &["write"],
    ),
    (
        "updateOne",
        "db.<coll>.updateOne(filter, update, options)",
        "Update one document",
        &["write"],
    ),
    (
        "updateMany",
        "db.<coll>.updateMany(filter, update, options)",
        "Update all matching documents",
        &["write", "dangerous"],
    ),
    (
        "replaceOne",
        "db.<coll>.replaceOne(filter, replacement, options)",
        "Replace one document",
        &["write"],
    ),
    (
        "deleteOne",
        "db.<coll>.deleteOne(filter)",
        "Delete one document",
        &["write"],
    ),
    (
        "deleteMany",
        "db.<coll>.deleteMany(filter)",
        "Delete all matching documents",
        &["write", "dangerous"],
    ),
    (
        "findOneAndUpdate",
        "db.<coll>.findOneAndUpdate(filter, update, options)",
        "Update and return a document",
        &["write"],
    ),
    (
        "findOneAndReplace",
        "db.<coll>.findOneAndReplace(filter, replacement, options)",
        "Replace and return a document",
        &["write"],
    ),
    (
        "findOneAndDelete",
        "db.<coll>.findOneAndDelete(filter, options)",
        "Delete and return a document",
        &["write"],
    ),
    (
        "bulkWrite",
        "db.<coll>.bulkWrite([operations], options)",
        "Run several write operations",
        &["write"],
    ),
    (
        "createIndex",
        "db.<coll>.createIndex(keys, options)",
        "Create an index",
        &["ddl"],
    ),
    (
        "createIndexes",
        "db.<coll>.createIndexes([specs])",
        "Create several indexes",
        &["ddl"],
    ),
    (
        "dropIndex",
        "db.<coll>.dropIndex(index)",
        "Drop an index",
        &["ddl"],
    ),
    (
        "dropIndexes",
        "db.<coll>.dropIndexes()",
        "Drop all indexes",
        &["ddl"],
    ),
    (
        "getIndexes",
        "db.<coll>.getIndexes()",
        "List the indexes of the collection",
        &["read"],
    ),
    (
        "drop",
        "db.<coll>.drop()",
        "Drop the collection",
        &["ddl", "dangerous"],
    ),
    (
        "renameCollection",
        "db.<coll>.renameCollection(newName, dropTarget)",
        "Rename the collection",
        &["ddl"],
    ),
    (
        "stats",
        "db.<coll>.stats()",
        "Collection statistics",
        &["read"],
    ),
    (
        "runCommand",
        "db.runCommand({command: 1})",
        "Run a database command",
        &["procedural"],
    ),
    (
        "adminCommand",
        "db.adminCommand({command: 1})",
        "Run a command against the admin database",
        &["procedural"],
    ),
    (
        "getCollectionNames",
        "db.getCollectionNames()",
        "List the collections of the database",
        &["read"],
    ),
    (
        "createCollection",
        "db.createCollection(name, options)",
        "Create a collection",
        &["ddl"],
    ),
    (
        "createView",
        "db.createView(name, source, pipeline)",
        "Create a view",
        &["ddl"],
    ),
    (
        "dropDatabase",
        "db.dropDatabase()",
        "Drop the current database",
        &["ddl", "dangerous"],
    ),
    ("sort", ".sort({field: 1})", "Sort the cursor", &["chain"]),
    (
        "limit",
        ".limit(n)",
        "Limit the number of documents",
        &["chain"],
    ),
    ("skip", ".skip(n)", "Skip documents", &["chain"]),
    (
        "project",
        ".project({field: 1})",
        "Project fields",
        &["chain"],
    ),
    ("hint", ".hint(index)", "Force an index", &["chain"]),
    (
        "collation",
        ".collation({locale: 'en'})",
        "Set the collation",
        &["chain"],
    ),
    ("maxTimeMS", ".maxTimeMS(ms)", "Time limit", &["chain"]),
    ("batchSize", ".batchSize(n)", "Batch size", &["chain"]),
    (
        "explain",
        ".explain('executionStats')",
        "Explain the query",
        &["chain"],
    ),
    (
        "count",
        ".count()",
        "Count the documents of the cursor",
        &["chain"],
    ),
    (
        "toArray",
        ".toArray()",
        "Return the cursor as an array",
        &["chain"],
    ),
];

/// Vocabulary of `d`: SQL keywords and functions, Redis commands (the table of
/// [`crate::redis_cli`]) or Mongo shell methods.
pub fn vocabulary(d: ScriptDialect) -> Vocabulary {
    match d {
        ScriptDialect::Postgres => Vocabulary {
            keywords: merged(&[BASE_KEYWORDS, PG_KEYWORDS]),
            functions: merged(&[BASE_FUNCTIONS, PG_FUNCTIONS]),
            commands: Vec::new(),
        },
        ScriptDialect::MySql { .. } => Vocabulary {
            keywords: merged(&[BASE_KEYWORDS, MYSQL_KEYWORDS]),
            functions: merged(&[BASE_FUNCTIONS, MYSQL_FUNCTIONS]),
            commands: Vec::new(),
        },
        ScriptDialect::MsSql => Vocabulary {
            keywords: merged(&[BASE_KEYWORDS, MSSQL_KEYWORDS]),
            functions: merged(&[BASE_FUNCTIONS, MSSQL_FUNCTIONS]),
            commands: Vec::new(),
        },
        ScriptDialect::Oracle => Vocabulary {
            keywords: merged(&[BASE_KEYWORDS, ORACLE_KEYWORDS]),
            functions: merged(&[BASE_FUNCTIONS, ORACLE_FUNCTIONS]),
            commands: Vec::new(),
        },
        ScriptDialect::RedisCli => Vocabulary {
            keywords: Vec::new(),
            functions: Vec::new(),
            commands: crate::redis_cli::COMMANDS
                .iter()
                .map(|c| CommandHint {
                    name: c.name.to_string(),
                    syntax: crate::redis_cli::syntax(c.name)
                        .unwrap_or(c.name)
                        .to_string(),
                    summary: c.summary.to_string(),
                    flags: flag_names(c.flags),
                })
                .collect(),
        },
        ScriptDialect::MongoShell => Vocabulary {
            keywords: ["use", "show", "db", "dbs", "collections"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            functions: [
                "ObjectId",
                "ISODate",
                "Date",
                "NumberInt",
                "NumberLong",
                "NumberDecimal",
                "UUID",
                "BinData",
                "Timestamp",
                "MinKey",
                "MaxKey",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            commands: MONGO_METHODS
                .iter()
                .map(|(name, syntax, summary, flags)| CommandHint {
                    name: name.to_string(),
                    syntax: syntax.to_string(),
                    summary: summary.to_string(),
                    flags: flags.iter().map(|f| f.to_string()).collect(),
                })
                .collect(),
        },
        // CQL has no JOIN / GROUP BY ... keep only the keywords that make sense for it.
        ScriptDialect::Cql => Vocabulary {
            keywords: merged(&[CQL_BASE_KEYWORDS, CQL_KEYWORDS]),
            functions: words(CQL_FUNCTIONS),
            commands: Vec::new(),
        },
        ScriptDialect::ClickHouse => Vocabulary {
            keywords: merged(&[BASE_KEYWORDS, CH_KEYWORDS]),
            functions: merged(&[BASE_FUNCTIONS, CH_FUNCTIONS]),
            commands: Vec::new(),
        },
        ScriptDialect::Opaque => Vocabulary::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::testing::write_or_check;
    use serde_json::json;

    fn hint() -> CommandHint {
        CommandHint {
            name: "SET".into(),
            syntax: "SET key value [EX seconds]".into(),
            summary: "Set the string value of a key".into(),
            flags: vec!["write".into()],
        }
    }

    fn sample() -> Vocabulary {
        Vocabulary {
            keywords: vec!["SELECT".into()],
            functions: vec!["count".into()],
            commands: vec![hint()],
        }
    }

    #[test]
    fn sql_vocabularies_have_keywords_and_functions() {
        for d in [
            ScriptDialect::Postgres,
            ScriptDialect::mysql(false, true),
            ScriptDialect::MsSql,
            ScriptDialect::Oracle,
        ] {
            let v = vocabulary(d);
            assert!(v.keywords.len() > 100, "{d:?}");
            assert!(v.functions.len() > 25, "{d:?}");
            assert!(v.commands.is_empty());
            assert!(v.keywords.contains(&"SELECT".to_string()));
            assert!(v.functions.contains(&"count".to_string()));
            let mut up: Vec<String> = v.keywords.iter().map(|k| k.to_ascii_uppercase()).collect();
            let n = up.len();
            up.sort();
            up.dedup();
            assert_eq!(up.len(), n, "duplicate keyword in {d:?}");
        }
        assert!(
            vocabulary(ScriptDialect::Postgres)
                .keywords
                .contains(&"RETURNING".to_string())
        );
        assert!(
            vocabulary(ScriptDialect::MsSql)
                .keywords
                .contains(&"GO".to_string())
        );
        assert!(
            vocabulary(ScriptDialect::Oracle)
                .functions
                .contains(&"nvl".to_string())
        );
    }

    #[test]
    fn redis_vocabulary_comes_from_the_command_table() {
        let v = vocabulary(ScriptDialect::RedisCli);
        assert!(v.keywords.is_empty() && v.functions.is_empty());
        assert_eq!(v.commands.len(), crate::redis_cli::COMMANDS.len());
        let set = v.commands.iter().find(|c| c.name == "SET").unwrap();
        assert!(set.syntax.starts_with("SET key value"));
        assert_eq!(set.flags, ["write"]);
        let fl = v.commands.iter().find(|c| c.name == "FLUSHALL").unwrap();
        assert!(fl.flags.contains(&"dangerous".to_string()));
    }

    #[test]
    fn mongo_vocabulary_lists_methods() {
        let v = vocabulary(ScriptDialect::MongoShell);
        for m in ["find", "insertOne", "deleteMany", "runCommand", "sort"] {
            assert!(v.commands.iter().any(|c| c.name == m), "{m}");
        }
        assert!(v.functions.contains(&"ObjectId".to_string()));
        let dm = v.commands.iter().find(|c| c.name == "deleteMany").unwrap();
        assert!(dm.flags.contains(&"dangerous".to_string()));
    }

    #[test]
    fn opaque_vocabulary_is_empty() {
        assert_eq!(vocabulary(ScriptDialect::Opaque), Vocabulary::default());
    }

    #[test]
    fn json_shapes() {
        let v = sample();
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(
            j,
            json!({
                "keywords": ["SELECT"],
                "functions": ["count"],
                "commands": [{
                    "name": "SET",
                    "syntax": "SET key value [EX seconds]",
                    "summary": "Set the string value of a key",
                    "flags": ["write"]
                }]
            })
        );
        assert_eq!(serde_json::from_value::<Vocabulary>(j).unwrap(), v);
    }

    #[test]
    fn fixtures() {
        write_or_check(
            "script",
            &json!({
                "Vocabulary": serde_json::to_value(sample()).unwrap(),
                "CommandHint": serde_json::to_value(hint()).unwrap(),
            }),
        );
    }
}
