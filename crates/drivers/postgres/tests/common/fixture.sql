DROP SCHEMA IF EXISTS dbs_it CASCADE;
DROP SCHEMA IF EXISTS "Dbs It ""S""" CASCADE;
CREATE SCHEMA dbs_it;
CREATE SCHEMA "Dbs It ""S""";

CREATE TYPE dbs_it.mood AS ENUM ('sad', 'ok', 'happy');
CREATE TYPE dbs_it.pair AS (a int, b text);
CREATE DOMAIN dbs_it.posint AS int CHECK (VALUE > 0);

CREATE TABLE dbs_it.all_types (
    id int PRIMARY KEY,
    c_i2 smallint, c_i4 int, c_i8 bigint, c_num numeric(38,10), c_f4 real, c_f8 double precision,
    c_bool boolean, c_text text, c_vc varchar(20), c_ch char(5), c_name name, c_bytea bytea,
    c_date date, c_time time, c_timetz timetz, c_ts timestamp, c_tstz timestamptz, c_uuid uuid,
    c_json json, c_jsonb jsonb,
    a_i4 int[], a_text text[], a_num numeric[], a_tstz timestamptz[], a_bool boolean[],
    c_enum dbs_it.mood, c_inet inet, c_cidr cidr, c_interval interval, c_range int4range,
    c_point point, c_comp dbs_it.pair, c_dom dbs_it.posint, c_money money, c_xml xml,
    c_bit bit(3), c_mac macaddr, a_enum dbs_it.mood[]
);

INSERT INTO dbs_it.all_types VALUES (
    1, 12, 123456, 9007199254740993, 1234567890123456789012345678.1234567890, 1.5, 2.25,
    true, 'héllo wörld', 'varchar', 'ab', 'nm', '\x00ff10',
    '2024-05-31', '13:45:00.5', '13:45:00+07', '2024-05-31 13:45:00.123456',
    '2024-05-31 13:45:00.5+07', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11',
    '{"a": [1, 2]}', '{"b": {"c": null}}',
    '{1,2,NULL}', '{a,"b c",NULL}', '{1.50,2}', '{"2024-01-01 00:00:00+00"}', '{t,f}',
    'happy', '10.0.0.1/8', '10.1.0.0/16', '1 day 02:03:04', '[1,10)',
    '(1,2)', '(1,"x")', 5, '$12.34', '<a>1</a>',
    B'101', '08:00:2b:01:02:03', '{sad,happy}'
);

INSERT INTO dbs_it.all_types (id, c_i2, c_i4, c_i8, c_num, c_f4, c_f8, c_text, c_bytea, c_date, c_ts,
                              c_tstz, c_json, c_jsonb, a_i4, a_text)
VALUES (2, -32768, -2147483648, 9223372036854775807, 9999999999999999999999999999.9999999999,
        'NaN', '-Infinity', '', '\x', 'infinity', '0001-01-01 00:00:00', 'infinity', '[]', 'null',
        '{}', '{"","NULL"}');

INSERT INTO dbs_it.all_types (id) VALUES (3);

INSERT INTO dbs_it.all_types (id, c_i8, c_num, c_f4, c_f8)
VALUES (4, -9223372036854775808, 'NaN', 'Infinity', 'NaN');

CREATE TABLE dbs_it.parents (id serial PRIMARY KEY, name text NOT NULL);
INSERT INTO dbs_it.parents (name) VALUES ('p1'), ('p2');
COMMENT ON TABLE dbs_it.parents IS 'Parent rows';

CREATE TABLE dbs_it.children (
    id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    parent_id int NOT NULL REFERENCES dbs_it.parents (id) ON DELETE CASCADE ON UPDATE RESTRICT,
    note text DEFAULT 'n/a',
    price numeric(10,2),
    total numeric GENERATED ALWAYS AS (price * 2) STORED,
    UNIQUE (parent_id, note),
    CHECK (price >= 0)
);
CREATE INDEX children_note_idx ON dbs_it.children (lower(note));
COMMENT ON COLUMN dbs_it.children.note IS 'free text';
INSERT INTO dbs_it.children (parent_id, note, price) VALUES (1, 'a', 1.5), (1, 'b', 2), (2, 'c', 3);

CREATE FUNCTION dbs_it.trg_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER children_before_ins BEFORE INSERT OR UPDATE ON dbs_it.children
    FOR EACH ROW EXECUTE FUNCTION dbs_it.trg_fn();

CREATE VIEW dbs_it.v_children AS SELECT id, note FROM dbs_it.children;
CREATE MATERIALIZED VIEW dbs_it.mv_parents AS SELECT * FROM dbs_it.parents;
CREATE INDEX mv_parents_name_idx ON dbs_it.mv_parents (name);

CREATE FUNCTION dbs_it.add(a int, b int) RETURNS int LANGUAGE sql AS 'select a + b';
CREATE FUNCTION dbs_it.add(a int, b int, c int) RETURNS int LANGUAGE sql AS 'select a + b + c';
CREATE PROCEDURE dbs_it.do_nothing() LANGUAGE plpgsql AS $$ BEGIN NULL; END $$;
CREATE SEQUENCE dbs_it.seq1 AS bigint START 5 INCREMENT 2 MINVALUE 1 MAXVALUE 1000 CACHE 3 CYCLE;

CREATE TABLE dbs_it.part (id int, d date) PARTITION BY RANGE (d);
CREATE TABLE dbs_it.part_2024 PARTITION OF dbs_it.part FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');

CREATE TABLE dbs_it.people (id int PRIMARY KEY, name text, age int, note text);
INSERT INTO dbs_it.people VALUES
    (1, 'Alice', 30, '100% sure'),
    (2, 'bob', 25, 'a_b'),
    (3, 'Carol', NULL, NULL),
    (4, 'dave', 40, 'O''Brien'),
    (5, 'Eve', 35, 'back\slash'),
    (6, 'ALICE2', 22, 'x'),
    (7, 'f_g%', 50, '100 sure');

CREATE TABLE dbs_it."Weird ""Table"" Name" (id int, "Col A" text, "we""ird" text);
INSERT INTO dbs_it."Weird ""Table"" Name" VALUES (1, 'z', 'one'), (2, 'a', 'two'), (3, 'm', 'three');
CREATE TABLE "Dbs It ""S"""."T x" (id int, "y z" int);
INSERT INTO "Dbs It ""S"""."T x" VALUES (1, 10), (2, 20);

CREATE TABLE dbs_it.big AS
    SELECT g::int AS id, md5(g::text) AS name, (g % 10)::int AS grp FROM generate_series(1, 200000) g;
ALTER TABLE dbs_it.big ADD PRIMARY KEY (id);
ANALYZE dbs_it.big;
