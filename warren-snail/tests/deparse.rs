//! The deparsers over a snail's catalog, each printing as PostgreSQL 18.6 prints the same object:
//! pg_get_expr over a DEFAULT, a generation, a CHECK and an index's trees, pg_get_constraintdef and
//! pg_get_indexdef, plain and pretty; pg_get_viewdef by OID and by name, plain, pretty and unwrapped;
//! pg_get_ruledef of a view's rule and of a built-in one; the pg_get_function_* functions of the file's
//! functions, a range's constructors and built-ins; a parse tree's column read only through
//! pg_get_expr. Every expected string was printed by 18.6 after the same DDL.

mod weather;

use arrow::array::{Array, StringArray};
use arrow::record_batch::RecordBatch;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::tmp;

const DDL: &str = "CREATE SEQUENCE w_id_seq; \
     CREATE TABLE t (id bigint PRIMARY KEY, n text NOT NULL DEFAULT 'x', v varchar(10) DEFAULT 'y', \
     c numeric(8,2) CHECK (c > 0), q integer DEFAULT -1, w bigint DEFAULT nextval('w_id_seq'), \
     g numeric GENERATED ALWAYS AS (q * c) STORED); \
     CREATE TABLE s.u (id bigint PRIMARY KEY, t_id bigint REFERENCES t (id) ON DELETE CASCADE, \
     k integer UNIQUE, CONSTRAINT k_small CHECK (k < 10 OR k IS NULL)); \
     CREATE DOMAIN s.mail AS text CHECK (position('@' in VALUE) > 1); \
     CREATE INDEX t_n ON t (n) WHERE c > 1; \
     CREATE INDEX t_v ON t (v varchar_pattern_ops DESC NULLS LAST, id) INCLUDE (n) \
     WITH (fillfactor = 90);";

fn opened(name: &str) -> Snail {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18).unwrap();
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

fn s(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|x| Some(x.to_string())).collect()
}

#[test]
fn a_default_and_a_generation_print_with_the_casts_postgresql_makes() {
    let mut snail = opened("dep-defaults");
    assert_eq!(
        rows(
            &mut snail,
            "SELECT a.attname::text, pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
             JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
             WHERE d.adrelid = 't'::regclass ORDER BY d.adnum"
        ),
        [
            s(&["n", "'x'::text"]),
            s(&["v", "'y'::character varying"]),
            s(&["q", "'-1'::integer"]),
            s(&["w", "nextval('w_id_seq'::regclass)"]),
            s(&["g", "((q)::numeric * c)"]),
        ]
    );
}

#[test]
fn a_constraint_prints_as_pg_get_constraintdef_prints_it_plain_and_pretty() {
    let mut snail = opened("dep-constraints");
    assert_eq!(
        rows(
            &mut snail,
            "SELECT conname::text, pg_get_constraintdef(oid), pg_get_constraintdef(oid, true) \
             FROM pg_constraint WHERE connamespace IN ('public'::regnamespace, 's'::regnamespace) \
             ORDER BY conname"
        ),
        [
            s(&["k_small", "CHECK (((k < 10) OR (k IS NULL)))", "CHECK (k < 10 OR k IS NULL)"]),
            s(&[
                "mail_check",
                "CHECK ((POSITION(('@'::text) IN (VALUE)) > 1))",
                "CHECK (POSITION(('@'::text) IN (VALUE)) > 1)"
            ]),
            s(&["t_c_check", "CHECK ((c > (0)::numeric))", "CHECK (c > 0::numeric)"]),
            s(&["t_id_not_null", "NOT NULL id", "NOT NULL id"]),
            s(&["t_n_not_null", "NOT NULL n", "NOT NULL n"]),
            s(&["t_pkey", "PRIMARY KEY (id)", "PRIMARY KEY (id)"]),
            s(&["u_id_not_null", "NOT NULL id", "NOT NULL id"]),
            s(&["u_k_key", "UNIQUE (k)", "UNIQUE (k)"]),
            s(&["u_pkey", "PRIMARY KEY (id)", "PRIMARY KEY (id)"]),
            s(&[
                "u_t_id_fkey",
                "FOREIGN KEY (t_id) REFERENCES t(id) ON DELETE CASCADE",
                "FOREIGN KEY (t_id) REFERENCES t(id) ON DELETE CASCADE"
            ]),
        ]
    );
}

#[test]
fn an_index_prints_as_pg_get_indexdef_prints_it() {
    let mut snail = opened("dep-indexes");
    assert_eq!(
        rows(
            &mut snail,
            "SELECT c.relname::text, pg_get_indexdef(i.indexrelid), \
             pg_get_indexdef(i.indexrelid, 0, true), pg_get_indexdef(i.indexrelid, 1, true), \
             pg_get_expr(i.indexprs, i.indrelid), pg_get_expr(i.indpred, i.indrelid, true) \
             FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname IN ('t_n', 't_v') ORDER BY 1"
        ),
        [
            vec![
                Some("t_n".into()),
                Some("CREATE INDEX t_n ON public.t USING btree (n) WHERE (c > (1)::numeric)".into()),
                Some("CREATE INDEX t_n ON t USING btree (n) WHERE c > 1::numeric".into()),
                Some("n".into()),
                None,
                Some("c > 1::numeric".into()),
            ],
            vec![
                Some("t_v".into()),
                Some(
                    "CREATE INDEX t_v ON public.t USING btree (v varchar_pattern_ops DESC NULLS LAST, \
                     id) INCLUDE (n) WITH (fillfactor='90')"
                        .into()
                ),
                Some(
                    "CREATE INDEX t_v ON t USING btree (v varchar_pattern_ops DESC NULLS LAST, id) \
                     INCLUDE (n) WITH (fillfactor='90')"
                        .into()
                ),
                Some("v".into()),
                None,
                None,
            ],
        ]
    );
}

#[test]
fn a_parse_tree_is_read_only_through_pg_get_expr() {
    let mut snail = opened("dep-trees");
    let refused = snail
        .ask("SELECT adbin FROM pg_attrdef", None)
        .err()
        .map(|e| e.0)
        .unwrap_or_default();
    assert!(refused.contains("a server's parse tree"), "{refused}");
    let e = snail
        .ask(
            "SELECT pg_get_expr(adbin, 0) FROM pg_attrdef WHERE adrelid = 't'::regclass AND adnum = 7",
            None,
        )
        .err()
        .map(|e| e.0)
        .unwrap_or_default();
    assert_eq!(e, "expression contains variables");
}

#[test]
fn a_columns_type_and_typmod_and_a_domains_base_are_postgresqls() {
    let path = tmp("dep-types.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE r (id integer PRIMARY KEY, room int4range, iv interval day, \
         ivs interval second(3)); CREATE DOMAIN ints AS integer[];",
    )
    .unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18).unwrap();
    let mut snail = Snail::open(&path).unwrap();
    assert_eq!(
        rows(
            &mut snail,
            "SELECT attname::text, atttypid::regtype::text, atttypmod::text FROM pg_attribute \
             WHERE attrelid = 'r'::regclass AND attnum > 0 ORDER BY attnum"
        ),
        [
            s(&["id", "integer", "-1"]),
            s(&["room", "int4range", "-1"]),
            s(&["iv", "interval", "589823"]),
            s(&["ivs", "interval", "268435459"]),
        ]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT typbasetype::regtype::text, typcategory::text FROM pg_type WHERE typname = 'ints'"
        ),
        [s(&["integer[]", "A"])]
    );
}

const VIEWS: &str = "CREATE TABLE t (id bigint PRIMARY KEY, n text NOT NULL DEFAULT 'x', q integer, \
     c numeric(8,2)); \
     CREATE TABLE s.u (id bigint PRIMARY KEY, t_id bigint REFERENCES t (id), k integer UNIQUE); \
     CREATE VIEW tv AS SELECT t.id, t.n FROM t WHERE t.c > 1; \
     CREATE VIEW s.uv (key, total) AS SELECT u.t_id, count(*) FROM s.u u JOIN t ON t.id = u.t_id \
     GROUP BY u.t_id; \
     CREATE VIEW s.unioned AS SELECT id FROM t UNION ALL SELECT k FROM s.u ORDER BY 1 LIMIT 3; \
     CREATE VIEW s.sub AS SELECT x.id FROM (SELECT t.id FROM t WHERE EXISTS \
     (SELECT 1 FROM t t2 WHERE t2.q = t.q)) x; \
     CREATE VIEW s.again AS SELECT t.id FROM t WHERE t.q IN (SELECT t.q FROM t); \
     CREATE VIEW s.ranked AS SELECT u.id, rank() OVER (ORDER BY u.k DESC) AS r FROM s.u u; \
     CREATE VIEW s.mixed AS (SELECT id FROM t UNION SELECT k FROM s.u) EXCEPT SELECT t_id FROM s.u; \
     CREATE VIEW s.dated AS SELECT t.id, now() - interval '1 day' AS since FROM t;";

fn opened_views(name: &str) -> Snail {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(VIEWS).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18).unwrap();
    Snail::open(&path).unwrap()
}

#[test]
fn a_view_prints_as_pg_get_viewdef_prints_it_plain_and_pretty() {
    let mut snail = opened_views("dep-views");
    let got = rows(
        &mut snail,
        "SELECT c.relname::text, pg_get_viewdef(c.oid), pg_get_viewdef(c.oid, true) FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind = 'v' \
         AND n.nspname IN ('public', 's') ORDER BY c.relname",
    );
    assert_eq!(
        got,
        vec![
            s(&[
                "again",
                " SELECT id\n   FROM t\n  WHERE (q IN ( SELECT t_1.q\n           FROM t t_1));",
                " SELECT id\n   FROM t\n  WHERE (q IN ( SELECT t_1.q\n           FROM t t_1));",
            ]),
            s(&[
                "dated",
                " SELECT id,\n    (now() - '1 day'::interval) AS since\n   FROM t;",
                " SELECT id,\n    now() - '1 day'::interval AS since\n   FROM t;",
            ]),
            s(&[
                "mixed",
                "(\n         SELECT t.id\n           FROM t\n        UNION\n         SELECT u.k AS id\n           FROM s.u\n) EXCEPT\n SELECT u.t_id AS id\n   FROM s.u;",
                "(\n         SELECT t.id\n           FROM t\n        UNION\n         SELECT u.k AS id\n           FROM s.u\n) EXCEPT\n SELECT u.t_id AS id\n   FROM s.u;",
            ]),
            s(&[
                "ranked",
                " SELECT id,\n    rank() OVER (ORDER BY k DESC) AS r\n   FROM s.u u;",
                " SELECT id,\n    rank() OVER (ORDER BY k DESC) AS r\n   FROM s.u u;",
            ]),
            s(&[
                "sub",
                " SELECT id\n   FROM ( SELECT t.id\n           FROM t\n          WHERE (EXISTS ( SELECT 1\n                   FROM t t2\n                  WHERE (t2.q = t.q)))) x;",
                " SELECT id\n   FROM ( SELECT t.id\n           FROM t\n          WHERE (EXISTS ( SELECT 1\n                   FROM t t2\n                  WHERE t2.q = t.q))) x;",
            ]),
            s(&[
                "tv",
                " SELECT id,\n    n\n   FROM t\n  WHERE (c > (1)::numeric);",
                " SELECT id,\n    n\n   FROM t\n  WHERE c > 1::numeric;",
            ]),
            s(&[
                "unioned",
                " SELECT t.id\n   FROM t\nUNION ALL\n SELECT u.k AS id\n   FROM s.u\n  ORDER BY 1\n LIMIT 3;",
                " SELECT t.id\n   FROM t\nUNION ALL\n SELECT u.k AS id\n   FROM s.u\n  ORDER BY 1\n LIMIT 3;",
            ]),
            s(&[
                "uv",
                " SELECT u.t_id AS key,\n    count(*) AS total\n   FROM (s.u u\n     JOIN t ON ((t.id = u.t_id)))\n  GROUP BY u.t_id;",
                " SELECT u.t_id AS key,\n    count(*) AS total\n   FROM s.u u\n     JOIN t ON t.id = u.t_id\n  GROUP BY u.t_id;",
            ]),
        ]
    );
}

#[test]
fn a_view_is_found_by_name_unwrapped_by_minus_one_and_a_table_has_no_definition_and_a_built_in_is_carried() {
    let mut snail = opened_views("dep-views-named");
    let got = rows(
        &mut snail,
        "SELECT pg_get_viewdef('s.uv'), pg_get_viewdef('tv', true), \
         pg_get_viewdef('s.uv'::regclass, -1), pg_get_viewdef('t'::regclass), \
         (SELECT definition FROM pg_views WHERE viewname = 'tv'), \
         (SELECT view_definition FROM information_schema.views WHERE table_name = 'tv'), \
         pg_get_viewdef('pg_available_extensions'), pg_get_viewdef('pg_catalog.pg_available_extensions', true)",
    );
    assert_eq!(
        got,
        vec![vec![
            Some(" SELECT u.t_id AS key,\n    count(*) AS total\n   FROM (s.u u\n     JOIN t ON ((t.id = u.t_id)))\n  GROUP BY u.t_id;".into()),
            Some(" SELECT id,\n    n\n   FROM t\n  WHERE c > 1::numeric;".into()),
            Some(" SELECT u.t_id AS key, count(*) AS total\n   FROM s.u u\n     JOIN t ON t.id = u.t_id\n  GROUP BY u.t_id;".into()),
            None,
            Some(" SELECT id,\n    n\n   FROM t\n  WHERE (c > (1)::numeric);".into()),
            Some(" SELECT id,\n    n\n   FROM t\n  WHERE (c > (1)::numeric);".into()),
            Some(" SELECT e.name,\n    e.default_version,\n    x.extversion AS installed_version,\n    e.comment\n   FROM (pg_available_extensions() e(name, default_version, comment)\n     LEFT JOIN pg_extension x ON ((e.name = x.extname)));".into()),
            Some(" SELECT e.name,\n    e.default_version,\n    x.extversion AS installed_version,\n    e.comment\n   FROM pg_available_extensions() e(name, default_version, comment)\n     LEFT JOIN pg_extension x ON e.name = x.extname;".into()),
        ]]
    );
}

#[test]
fn a_views_rule_prints_as_pg_get_ruledef_prints_it_and_a_built_in_rule_is_carried() {
    let mut snail = opened_views("dep-rules");
    let got = rows(
        &mut snail,
        "SELECT pg_get_ruledef(r.oid), pg_get_ruledef(r.oid, true) FROM pg_rewrite r \
         WHERE r.ev_class = 'tv'::regclass OR r.rulename = 'pg_settings_n' ORDER BY r.rulename",
    );
    assert_eq!(
        got,
        vec![
            s(&[
                "CREATE RULE \"_RETURN\" AS\n    ON SELECT TO public.tv DO INSTEAD  SELECT id,\n    n\n   FROM t\n  WHERE (c > (1)::numeric);",
                "CREATE RULE \"_RETURN\" AS\n    ON SELECT TO tv DO INSTEAD  SELECT id,\n    n\n   FROM t\n  WHERE c > 1::numeric;",
            ]),
            s(&[
                "CREATE RULE pg_settings_n AS\n    ON UPDATE TO pg_catalog.pg_settings DO INSTEAD NOTHING;",
                "CREATE RULE pg_settings_n AS\n    ON UPDATE TO pg_settings DO INSTEAD NOTHING;",
            ]),
        ]
    );
}

const FUNCTIONS: &str = "CREATE FUNCTION s.half(numeric) RETURNS numeric LANGUAGE sql STABLE STRICT \
     PARALLEL RESTRICTED AS $$SELECT $1 / 2$$; \
     CREATE FUNCTION s.quoted(text) RETURNS text LANGUAGE sql IMMUTABLE RETURNS NULL ON NULL INPUT \
     AS 'SELECT $1 || ''$function$'''; \
     CREATE TYPE s.fr AS RANGE (subtype = float8);";

#[test]
fn a_function_and_a_ranges_constructors_print_as_pg_get_functiondef_prints_them() {
    let path = tmp("dep-functions.snail");
    let catalog = Catalog::parse(FUNCTIONS).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18).unwrap();
    let mut snail = Snail::open(&path).unwrap();
    let got = rows(
        &mut snail,
        "SELECT p.proname::text, pg_get_function_identity_arguments(p.oid), pg_get_function_result(p.oid), \
         pg_get_functiondef(p.oid), p.prosrc, p.provolatile::text, p.proisstrict::text, \
         p.proparallel::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 's' ORDER BY p.oid",
    );
    let f = |name: &str, args: &str, ret: &str, def: &str, src: &str, v: &str, strict: &str, par: &str| {
        s(&[name, args, ret, def, src, v, strict, par])
    };
    assert_eq!(
        got,
        vec![
            f("half", "numeric", "numeric", "CREATE OR REPLACE FUNCTION s.half(numeric)\n RETURNS numeric\n LANGUAGE sql\n STABLE PARALLEL RESTRICTED STRICT\nAS $function$SELECT $1 / 2$function$\n", "SELECT $1 / 2", "s", "true", "r"),
            f("quoted", "text", "text", "CREATE OR REPLACE FUNCTION s.quoted(text)\n RETURNS text\n LANGUAGE sql\n IMMUTABLE STRICT\nAS $functionx$SELECT $1 || '$function$'$functionx$\n", "SELECT $1 || '$function$'", "i", "true", "u"),
            f("fr", "double precision, double precision", "s.fr", "CREATE OR REPLACE FUNCTION s.fr(double precision, double precision)\n RETURNS s.fr\n LANGUAGE internal\n IMMUTABLE PARALLEL SAFE\nAS $function$range_constructor2$function$\n", "range_constructor2", "i", "false", "s"),
            f("fr", "double precision, double precision, text", "s.fr", "CREATE OR REPLACE FUNCTION s.fr(double precision, double precision, text)\n RETURNS s.fr\n LANGUAGE internal\n IMMUTABLE PARALLEL SAFE\nAS $function$range_constructor3$function$\n", "range_constructor3", "i", "false", "s"),
            f("fr_multirange", "", "s.fr_multirange", "CREATE OR REPLACE FUNCTION s.fr_multirange()\n RETURNS s.fr_multirange\n LANGUAGE internal\n IMMUTABLE PARALLEL SAFE STRICT\nAS $function$multirange_constructor0$function$\n", "multirange_constructor0", "i", "true", "s"),
            f("fr_multirange", "s.fr", "s.fr_multirange", "CREATE OR REPLACE FUNCTION s.fr_multirange(s.fr)\n RETURNS s.fr_multirange\n LANGUAGE internal\n IMMUTABLE PARALLEL SAFE STRICT\nAS $function$multirange_constructor1$function$\n", "multirange_constructor1", "i", "true", "s"),
            f("fr_multirange", "VARIADIC s.fr[]", "s.fr_multirange", "CREATE OR REPLACE FUNCTION s.fr_multirange(VARIADIC s.fr[])\n RETURNS s.fr_multirange\n LANGUAGE internal\n IMMUTABLE PARALLEL SAFE STRICT\nAS $function$multirange_constructor2$function$\n", "multirange_constructor2", "i", "true", "s"),
        ]
    );
    let got = rows(
        &mut snail,
        "SELECT pg_get_function_arguments('make_interval'::regproc), \
         pg_get_function_arg_default('make_interval'::regproc, 6), pg_get_functiondef('int4abs'::regproc), \
         (SELECT pg_get_function_sqlbody(p.oid) FROM pg_proc p WHERE p.proname = 'timedate_pl')",
    );
    assert_eq!(
        got,
        vec![s(&[
            "years integer DEFAULT 0, months integer DEFAULT 0, weeks integer DEFAULT 0, days integer DEFAULT 0, hours integer DEFAULT 0, mins integer DEFAULT 0, secs double precision DEFAULT 0.0",
            "0",
            "CREATE OR REPLACE FUNCTION pg_catalog.int4abs(integer)\n RETURNS integer\n LANGUAGE internal\n IMMUTABLE PARALLEL SAFE STRICT\nAS $function$int4abs$function$\n",
            "RETURN ($2 + $1)",
        ])]
    );
}
