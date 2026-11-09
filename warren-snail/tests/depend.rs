//! pg_depend over a snail's catalog: each of the file's objects depends on what PostgreSQL 18.6
//! records it depends on, by the same kind of dependency; information_schema's views over it name
//! what 18.6 names; a view's rule depends on its own view before 16 and not from 16; a domain, a
//! range or a composite naming a collation the file does not hold is refused as PostgreSQL refuses
//! it. Every expected row was given by PostgreSQL after the same DDL (15.19 and 16.15 for the
//! rules), each row of pg_depend by the names of its two objects, since the OIDs differ: a trigger
//! by its table, its constraint and the function it calls. A constraint's triggers are in
//! pg_trigger as 18.6 makes them, each named by its own OID.

mod weather;

use arrow::array::{Array, StringArray};
use arrow::record_batch::RecordBatch;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::tmp;

const DDL: &str = "CREATE SCHEMA s; CREATE COLLATION s.bytes (provider = libc, locale = 'C'); \
     CREATE TYPE s.mood AS ENUM ('sad', 'ok'); \
     CREATE TYPE s.pair AS (a integer, b text COLLATE \"C\"); \
     CREATE TYPE s.span AS RANGE (subtype = float8, subtype_diff = float8mi); \
     CREATE DOMAIN s.code AS text COLLATE \"C\" CHECK (VALUE <> ''); \
     CREATE SEQUENCE s.n_seq; \
     CREATE TABLE s.t (id bigint PRIMARY KEY, n integer DEFAULT nextval('s.n_seq'), m s.mood, \
     c s.code, p s.pair, r s.span, b text COLLATE s.bytes, g integer GENERATED ALWAYS AS (n * 2) STORED, \
     CHECK (n > 0)); \
     ALTER SEQUENCE s.n_seq OWNED BY s.t.n; \
     CREATE TABLE s.u (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, t_id bigint REFERENCES s.t (id), \
     k text UNIQUE); \
     CREATE INDEX t_b ON s.t (b) INCLUDE (m) WHERE n > 1; \
     CREATE VIEW s.v AS SELECT t.id, u.k FROM s.t JOIN s.u ON u.t_id = t.id; \
     CREATE VIEW s.w AS SELECT v.k FROM s.v;";

/// Each row of pg_depend touching the file's objects, by the names of its two objects and how one
/// depends on the other, with how many such rows there are.
const NAMED: &str = "SELECT x.k AS k, count(*) AS n FROM (SELECT CASE d.classid \
     WHEN 'pg_class'::regclass THEN 'class ' || (SELECT s.nspname || '.' || c.relname FROM pg_class c JOIN pg_namespace s ON s.oid = c.relnamespace WHERE c.oid = d.objid) || CASE WHEN d.objsubid <> 0 THEN '.' || (SELECT a.attname FROM pg_attribute a WHERE a.attrelid = d.objid AND a.attnum = d.objsubid) ELSE '' END \
     WHEN 'pg_type'::regclass THEN 'type ' || (SELECT s.nspname || '.' || t.typname FROM pg_type t JOIN pg_namespace s ON s.oid = t.typnamespace WHERE t.oid = d.objid) \
     WHEN 'pg_constraint'::regclass THEN 'constraint ' || (SELECT s.nspname || '.' || coalesce(c.relname, t.typname) || '.' || k.conname FROM pg_constraint k JOIN pg_namespace s ON s.oid = k.connamespace LEFT JOIN pg_class c ON c.oid = k.conrelid LEFT JOIN pg_type t ON t.oid = k.contypid WHERE k.oid = d.objid) \
     WHEN 'pg_attrdef'::regclass THEN 'default ' || (SELECT s.nspname || '.' || c.relname || '.' || a.attname FROM pg_attrdef f JOIN pg_class c ON c.oid = f.adrelid JOIN pg_namespace s ON s.oid = c.relnamespace JOIN pg_attribute a ON a.attrelid = f.adrelid AND a.attnum = f.adnum WHERE f.oid = d.objid) \
     WHEN 'pg_proc'::regclass THEN 'function ' || (SELECT s.nspname || '.' || p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')' FROM pg_proc p JOIN pg_namespace s ON s.oid = p.pronamespace WHERE p.oid = d.objid) \
     WHEN 'pg_rewrite'::regclass THEN 'rule ' || (SELECT s.nspname || '.' || c.relname || '.' || r.rulename FROM pg_rewrite r JOIN pg_class c ON c.oid = r.ev_class JOIN pg_namespace s ON s.oid = c.relnamespace WHERE r.oid = d.objid) \
     WHEN 'pg_cast'::regclass THEN 'cast ' || (SELECT k.castsource::regtype::text || ' AS ' || k.casttarget::regtype::text FROM pg_cast k WHERE k.oid = d.objid) \
     WHEN 'pg_collation'::regclass THEN 'collation ' || (SELECT s.nspname || '.' || l.collname FROM pg_collation l JOIN pg_namespace s ON s.oid = l.collnamespace WHERE l.oid = d.objid) \
     WHEN 'pg_trigger'::regclass THEN 'trigger ' || (SELECT s.nspname || '.' || c.relname || '.' || k.conname || ' ' || g.tgfoid::regproc::text FROM pg_trigger g JOIN pg_class c ON c.oid = g.tgrelid JOIN pg_namespace s ON s.oid = c.relnamespace JOIN pg_constraint k ON k.oid = g.tgconstraint WHERE g.oid = d.objid) \
     ELSE d.classid::regclass::text || ' ' || d.objid::text END || ' > ' || CASE d.refclassid \
     WHEN 'pg_class'::regclass THEN 'class ' || (SELECT s.nspname || '.' || c.relname FROM pg_class c JOIN pg_namespace s ON s.oid = c.relnamespace WHERE c.oid = d.refobjid) || CASE WHEN d.refobjsubid <> 0 THEN '.' || (SELECT a.attname FROM pg_attribute a WHERE a.attrelid = d.refobjid AND a.attnum = d.refobjsubid) ELSE '' END \
     WHEN 'pg_type'::regclass THEN 'type ' || (SELECT s.nspname || '.' || t.typname FROM pg_type t JOIN pg_namespace s ON s.oid = t.typnamespace WHERE t.oid = d.refobjid) \
     WHEN 'pg_constraint'::regclass THEN 'constraint ' || (SELECT s.nspname || '.' || coalesce(c.relname, t.typname) || '.' || k.conname FROM pg_constraint k JOIN pg_namespace s ON s.oid = k.connamespace LEFT JOIN pg_class c ON c.oid = k.conrelid LEFT JOIN pg_type t ON t.oid = k.contypid WHERE k.oid = d.refobjid) \
     WHEN 'pg_proc'::regclass THEN 'function ' || (SELECT s.nspname || '.' || p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')' FROM pg_proc p JOIN pg_namespace s ON s.oid = p.pronamespace WHERE p.oid = d.refobjid) \
     WHEN 'pg_namespace'::regclass THEN 'schema ' || (SELECT s.nspname FROM pg_namespace s WHERE s.oid = d.refobjid) \
     WHEN 'pg_collation'::regclass THEN 'collation ' || (SELECT s.nspname || '.' || l.collname FROM pg_collation l JOIN pg_namespace s ON s.oid = l.collnamespace WHERE l.oid = d.refobjid) \
     ELSE d.refclassid::regclass::text || ' ' || d.refobjid::text END || ' ' || d.deptype::text AS k \
     FROM pg_depend d WHERE d.objid >= 16384 OR d.refobjid >= 16384) x GROUP BY x.k";

fn opened(name: &str, ddl: &str, release: u8) -> Snail {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), release).unwrap();
    Snail::open(&path).unwrap()
}

fn rows(s: &mut Snail, q: &str) -> Vec<Vec<Option<String>>> {
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
    text_rows(&b)
}

fn text_rows(b: &RecordBatch) -> Vec<Vec<Option<String>>> {
    let cols: Vec<&StringArray> = b
        .columns()
        .iter()
        .map(|a| a.as_any().downcast_ref::<StringArray>().expect("a text column"))
        .collect();
    (0..b.num_rows())
        .map(|r| cols.iter().map(|c| c.is_valid(r).then(|| c.value(r).to_string())).collect())
        .collect()
}

fn texts(v: &[Vec<Option<String>>]) -> Vec<String> {
    v.iter().map(|r| r.iter().map(|x| x.clone().unwrap_or_default()).collect::<Vec<_>>().join("|")).collect()
}

#[test]
fn each_object_depends_on_what_postgresql_records_it_depends_on() {
    let mut snail = opened("depend-rows", DDL, 18);
    // 18.6's rows less those of the TOAST tables it makes and of a foreign key's triggers.
    let got = rows(&mut snail, &format!("SELECT k, n::text FROM ({NAMED}) z ORDER BY k"));
    let expected = [
        "cast s.span AS s.span_multirange > function s.span_multirange(s.span) i",
        "cast s.span AS s.span_multirange > type s.span i",
        "cast s.span AS s.span_multirange > type s.span_multirange i",
        "class s.n_seq > class s.t.n a",
        "class s.n_seq > schema s n",
        "class s.pair > type s.pair i",
        "class s.t > schema s n",
        "class s.t.b > collation s.bytes n",
        "class s.t.c > type s.code n",
        "class s.t.m > type s.mood n",
        "class s.t.p > type s.pair n",
        "class s.t.r > type s.span n",
        "class s.t_b > class s.t.b a",
        "class s.t_b > class s.t.m a",
        "class s.t_b > class s.t.n a",
        "class s.t_b > collation s.bytes n",
        "class s.t_pkey > constraint s.t.t_pkey i",
        "class s.u > schema s n",
        "class s.u_id_seq > class s.u.id i",
        "class s.u_id_seq > schema s n",
        "class s.u_k_key > constraint s.u.u_k_key i",
        "class s.u_pkey > constraint s.u.u_pkey i",
        "class s.v > schema s n",
        "class s.w > schema s n",
        "collation s.bytes > schema s n",
        "constraint s.code.code_check > type s.code a",
        "constraint s.t.t_id_not_null > class s.t.id a",
        "constraint s.t.t_n_check > class s.t.n a",
        "constraint s.t.t_n_check > class s.t.n n",
        "constraint s.t.t_pkey > class s.t.id a",
        "constraint s.u.u_id_not_null > class s.u.id a",
        "constraint s.u.u_k_key > class s.u.k a",
        "constraint s.u.u_pkey > class s.u.id a",
        "constraint s.u.u_t_id_fkey > class s.t.id n",
        "constraint s.u.u_t_id_fkey > class s.t_pkey n",
        "constraint s.u.u_t_id_fkey > class s.u.t_id a",
        "default s.t.g > class s.t.g i",
        "default s.t.g > class s.t.n n",
        "default s.t.n > class s.n_seq n",
        "default s.t.n > class s.t.n a",
        "function s.span(double precision, double precision) > schema s n",
        "function s.span(double precision, double precision) > type s.span i",
        "function s.span(double precision, double precision) > type s.span n",
        "function s.span(double precision, double precision, text) > schema s n",
        "function s.span(double precision, double precision, text) > type s.span i",
        "function s.span(double precision, double precision, text) > type s.span n",
        "function s.span_multirange() > schema s n",
        "function s.span_multirange() > type s.span_multirange i",
        "function s.span_multirange() > type s.span_multirange n",
        "function s.span_multirange(VARIADIC s.span[]) > schema s n",
        "function s.span_multirange(VARIADIC s.span[]) > type s._span n",
        "function s.span_multirange(VARIADIC s.span[]) > type s.span_multirange i",
        "function s.span_multirange(VARIADIC s.span[]) > type s.span_multirange n",
        "function s.span_multirange(s.span) > schema s n",
        "function s.span_multirange(s.span) > type s.span n",
        "function s.span_multirange(s.span) > type s.span_multirange i",
        "function s.span_multirange(s.span) > type s.span_multirange n",
        "rule s.v._RETURN > class s.t.id n",
        "rule s.v._RETURN > class s.u.k n",
        "rule s.v._RETURN > class s.u.t_id n",
        "rule s.v._RETURN > class s.v i",
        "rule s.w._RETURN > class s.v.k n",
        "rule s.w._RETURN > class s.w i",
        "trigger s.t.u_t_id_fkey \"RI_FKey_noaction_del\" > constraint s.u.u_t_id_fkey i",
        "trigger s.t.u_t_id_fkey \"RI_FKey_noaction_upd\" > constraint s.u.u_t_id_fkey i",
        "trigger s.u.u_t_id_fkey \"RI_FKey_check_ins\" > constraint s.u.u_t_id_fkey i",
        "trigger s.u.u_t_id_fkey \"RI_FKey_check_upd\" > constraint s.u.u_t_id_fkey i",
        "type s._code > type s.code i",
        "type s._mood > type s.mood i",
        "type s._pair > type s.pair i",
        "type s._span > type s.span i",
        "type s._span_multirange > type s.span_multirange i",
        "type s._t > type s.t i",
        "type s._u > type s.u i",
        "type s._v > type s.v i",
        "type s._w > type s.w i",
        "type s.code > schema s n",
        "type s.mood > schema s n",
        "type s.pair > schema s n",
        "type s.span > schema s n",
        "type s.span_multirange > schema s n",
        "type s.span_multirange > type s.span i",
        "type s.t > class s.t i",
        "type s.u > class s.u i",
        "type s.v > class s.v i",
        "type s.w > class s.w i",
    ];
    let expected: Vec<String> = expected.iter().map(|k| format!("{k}|1")).collect();
    assert_eq!(texts(&got), expected);
}

#[test]
fn information_schemas_views_over_pg_depend_name_what_postgresql_names() {
    let mut snail = opened("depend-views", DDL, 18);
    let got = rows(
        &mut snail,
        "SELECT view_name||'.'||table_name||'.'||column_name FROM information_schema.view_column_usage \
         WHERE view_schema = 's' ORDER BY 1",
    );
    assert_eq!(texts(&got), ["v.t.id", "v.u.k", "v.u.t_id", "w.v.k"]);
    let got = rows(
        &mut snail,
        "SELECT constraint_name||'.'||column_name FROM information_schema.constraint_column_usage \
         WHERE table_schema = 's' ORDER BY 1",
    );
    assert_eq!(
        texts(&got),
        [
            "t_id_not_null.id",
            "t_n_check.n",
            "t_pkey.id",
            "u_id_not_null.id",
            "u_k_key.k",
            "u_pkey.id",
            "u_t_id_fkey.id",
        ]
    );
    let got = rows(
        &mut snail,
        "SELECT constraint_name::text, unique_constraint_name::text FROM \
         information_schema.referential_constraints WHERE constraint_schema = 's'",
    );
    assert_eq!(texts(&got), ["u_t_id_fkey|t_pkey"]);
    // The collations the rows name: a column's COLLATE's, a domain column's its domain's, the
    // domain's and its array's its COLLATE's.
    let got = rows(
        &mut snail,
        "SELECT a.attname::text || ' ' || coalesce((SELECT l.collname::text FROM pg_collation l \
         WHERE l.oid = a.attcollation), '-') FROM pg_attribute a WHERE a.attrelid = 's.t'::regclass \
         AND a.attnum > 0 ORDER BY a.attnum",
    );
    assert_eq!(texts(&got), ["id -", "n -", "m -", "c C", "p -", "r -", "b bytes", "g -"]);
    let got = rows(
        &mut snail,
        "SELECT t.typname::text || ' ' || coalesce((SELECT l.collname::text FROM pg_collation l \
         WHERE l.oid = t.typcollation), '-') FROM pg_type t WHERE t.typname IN ('code', '_code') \
         AND t.typnamespace = 's'::regnamespace ORDER BY 1",
    );
    assert_eq!(texts(&got), ["_code C", "code C"]);
}

#[test]
fn a_views_rule_depends_on_its_own_view_before_16_and_not_from_16() {
    let ddl = "CREATE TABLE t (id integer PRIMARY KEY, n text); CREATE VIEW v AS SELECT id FROM t;";
    let q = "SELECT c.relname::text || '.' || d.refobjsubid::text || ' ' || d.deptype::text FROM pg_depend d \
             JOIN pg_rewrite r ON r.oid = d.objid JOIN pg_class c ON c.oid = d.refobjid \
             WHERE d.classid = 'pg_rewrite'::regclass AND d.objid >= 16384 ORDER BY 1";
    let mut fifteen = opened("depend-rule-15", ddl, 15);
    assert_eq!(texts(&rows(&mut fifteen, q)), ["t.1 n", "v.0 i", "v.0 n"]);
    let mut sixteen = opened("depend-rule-16", ddl, 16);
    assert_eq!(texts(&rows(&mut sixteen, q)), ["t.1 n", "v.0 i"]);
}

#[test]
fn a_using_join_reads_its_merged_columns_and_a_checks_keys_are_its_columns_as_met() {
    let ddl = "CREATE TABLE a (id integer PRIMARY KEY, x text, CHECK (x <> '' AND id > 0 AND x <> 'y')); \
               CREATE TABLE b (id integer PRIMARY KEY, y text); \
               CREATE VIEW j AS SELECT * FROM a JOIN b USING (id);";
    let mut snail = opened("depend-using", ddl, 18);
    let q = "SELECT c.relname::text || '.' || d.refobjsubid::text || ' ' || d.deptype::text FROM pg_depend d \
             JOIN pg_rewrite r ON r.oid = d.objid JOIN pg_class c ON c.oid = d.refobjid \
             WHERE d.classid = 'pg_rewrite'::regclass AND d.objid >= 16384 ORDER BY 1";
    assert_eq!(texts(&rows(&mut snail, q)), ["a.1 n", "a.2 n", "b.1 n", "b.2 n", "j.0 i"]);
    let q = "SELECT conname::text || ' ' || conkey::text FROM pg_constraint WHERE contype = 'c' \
             AND conrelid = 'a'::regclass";
    assert_eq!(texts(&rows(&mut snail, q)), ["a_check {2,1}"]);
}

#[test]
fn a_type_naming_a_collation_the_file_does_not_hold_is_refused_as_postgresql_refuses_it() {
    for ddl in [
        "CREATE DOMAIN d AS text COLLATE nowhere",
        "CREATE TYPE r AS RANGE (subtype = text, collation = nowhere)",
        "CREATE TYPE c AS (a text COLLATE nowhere)",
    ] {
        let e = Catalog::parse(ddl).unwrap_err().0;
        assert!(e.contains("collation \"nowhere\" for encoding \"UTF8\" does not exist"), "{ddl}: {e}");
    }
    Catalog::parse("CREATE DOMAIN d AS text COLLATE \"C\"").unwrap();
}

#[test]
fn a_constraints_triggers_are_in_pg_trigger_as_postgresql_makes_them() {
    let ddl = "CREATE SCHEMA tr; \
         CREATE TABLE tr.p (id integer PRIMARY KEY, d integer, CONSTRAINT p_d UNIQUE (d) DEFERRABLE); \
         CREATE TABLE tr.pu (id integer PRIMARY KEY, u integer, CONSTRAINT pu_u UNIQUE (u)); \
         CREATE TABLE tr.pr (id integer PRIMARY KEY); \
         CREATE TABLE tr.c (id integer PRIMARY KEY, p integer REFERENCES tr.p (id) ON DELETE CASCADE \
         ON UPDATE SET NULL, q integer, CONSTRAINT c_q FOREIGN KEY (q) REFERENCES tr.pu (u) DEFERRABLE \
         INITIALLY DEFERRED, r integer, CONSTRAINT c_ne FOREIGN KEY (r) REFERENCES tr.p NOT ENFORCED, \
         CONSTRAINT c_restrict FOREIGN KEY (r) REFERENCES tr.pr ON DELETE RESTRICT DEFERRABLE); \
         CREATE TABLE tr.e (k integer PRIMARY KEY, id integer, EXCLUDE USING btree (id WITH =) DEFERRABLE); \
         CREATE TABLE tr.s2 (a integer PRIMARY KEY DEFERRABLE);";
    let mut snail = opened("triggers", ddl, 18);
    // Each trigger by its table, constraint and function: its name's prefix, whether the name ends
    // in its own OID, its type, the other table, the key's index, its deferral, and whether its OID
    // follows its constraint's. A NOT ENFORCED key makes none; an action other than NO ACTION is
    // never deferrable.
    let got = rows(
        &mut snail,
        "SELECT c.relname||'.'||k.conname||' '||g.tgfoid::regproc::text||'|'||regexp_replace(g.tgname, '[0-9]+$', '')\
         ||'|'||(g.tgname = regexp_replace(g.tgname, '[0-9]+$', '')||g.oid::text)::text||'|'||g.tgtype::text\
         ||'|'||g.tgconstrrelid::regclass::text||'|'||g.tgconstrindid::regclass::text||'|'||g.tgdeferrable::text\
         ||'|'||g.tginitdeferred::text||'|'||(g.oid::bigint > g.tgconstraint::bigint)::text \
         FROM pg_trigger g JOIN pg_class c ON c.oid = g.tgrelid JOIN pg_constraint k ON k.oid = g.tgconstraint \
         WHERE c.relnamespace = 'tr'::regnamespace ORDER BY 1",
    );
    assert_eq!(
        texts(&got),
        [
            "c.c_p_fkey \"RI_FKey_check_ins\"|RI_ConstraintTrigger_c_|true|5|tr.p|tr.p_pkey|false|false|true",
            "c.c_p_fkey \"RI_FKey_check_upd\"|RI_ConstraintTrigger_c_|true|17|tr.p|tr.p_pkey|false|false|true",
            "c.c_q \"RI_FKey_check_ins\"|RI_ConstraintTrigger_c_|true|5|tr.pu|tr.pu_u|true|true|true",
            "c.c_q \"RI_FKey_check_upd\"|RI_ConstraintTrigger_c_|true|17|tr.pu|tr.pu_u|true|true|true",
            "c.c_restrict \"RI_FKey_check_ins\"|RI_ConstraintTrigger_c_|true|5|tr.pr|tr.pr_pkey|true|false|true",
            "c.c_restrict \"RI_FKey_check_upd\"|RI_ConstraintTrigger_c_|true|17|tr.pr|tr.pr_pkey|true|false|true",
            "e.e_id_excl unique_key_recheck|Unique_ConstraintTrigger_|true|21|-|tr.e_id_excl|true|false|true",
            "p.c_p_fkey \"RI_FKey_cascade_del\"|RI_ConstraintTrigger_a_|true|9|tr.c|tr.p_pkey|false|false|true",
            "p.c_p_fkey \"RI_FKey_setnull_upd\"|RI_ConstraintTrigger_a_|true|17|tr.c|tr.p_pkey|false|false|true",
            "p.p_d unique_key_recheck|Unique_ConstraintTrigger_|true|21|-|tr.p_d|true|false|true",
            "pr.c_restrict \"RI_FKey_noaction_upd\"|RI_ConstraintTrigger_a_|true|17|tr.c|tr.pr_pkey|true|false|true",
            "pr.c_restrict \"RI_FKey_restrict_del\"|RI_ConstraintTrigger_a_|true|9|tr.c|tr.pr_pkey|false|false|true",
            "pu.c_q \"RI_FKey_noaction_del\"|RI_ConstraintTrigger_a_|true|9|tr.c|tr.pu_u|true|true|true",
            "pu.c_q \"RI_FKey_noaction_upd\"|RI_ConstraintTrigger_a_|true|17|tr.c|tr.pu_u|true|true|true",
            "s2.s2_pkey unique_key_recheck|PK_ConstraintTrigger_|true|21|-|tr.s2_pkey|true|false|true",
        ]
    );
    // Each one depends on its constraint, internally.
    let got = rows(
        &mut snail,
        "SELECT count(*)::text FROM pg_depend d JOIN pg_trigger g ON g.oid = d.objid \
         WHERE d.classid = 'pg_trigger'::regclass AND d.refobjid = g.tgconstraint AND d.deptype = 'i'",
    );
    assert_eq!(texts(&got), ["15"]);
}
