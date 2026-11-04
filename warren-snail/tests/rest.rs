//! The functions no other family holds, each in a pipeline as PostgreSQL 19 computes it: the
//! geometric types', the network addresses', the bit strings', the enums', the uuids', the fixed
//! settings' and the rest; each call resolved to a form as PostgreSQL resolves it, or refused with
//! PostgreSQL's message. Each answer below is PostgreSQL 19's (RC1, 2026-10-31) over the same rows.

#![cfg(feature = "standalone")]

mod weather;

use arrow::array::{Array, AsArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::write::Rows;
use weather::{tmp, write};

const DDL: &str = "CREATE TYPE s.mood AS ENUM ('sad', 'ok', 'happy'); \
    CREATE TABLE s.t (id integer NOT NULL, p point, q point, b box, c circle, pa path, sg lseg, \
    i inet, ci cidr, m macaddr, m8 macaddr8, a bit varying, bb bit varying, e s.mood, u uuid, \
    sn pg_snapshot, t tid, o oid, n integer, PRIMARY KEY (id));";

const ROWS: &str = "INSERT INTO s.t VALUES (1, '(1,2)', '(4,6)', '(3,4),(1,2)', '<(1,1),2>', '((0,0),(4,0),(4,3))', '[(0,0),(3,4)]', '192.168.1.5/20', '10.1.0.0/17', '08:00:2b:01:02:03', '08:00:2b:01:02:03:04:05', B'1011001110', B'0011', 'ok', '017f22e2-79b0-7cc3-98c4-dc0c0c07398f', '10:20:11,12,15', '(7,9)', 1043, 7), (2, '(1,2)', '(1,2)', '(0,0),(0,0)', '<(0,0),0>', '[(0,0),(3,4)]', '[(1,1),(1,5)]', '2001:db8::1/64', '0:0:1:0:1:0:0:0/128', 'ff:ff:ff:ff:ff:ff', 'ff:ff:ff:ff:ff:ff:ff:ff', B'111111110', B'10', 'happy', 'a0eebc99-9c0b-11d1-8000-00aa00000000', '1:1:', '(4294967295,65535)', 1007, -1), (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 2147483647);";

const COLUMNS: &str = "id, p, q, b, c, pa, sg, i, ci, m, m8, a, bb, e, u, sn, t, o, n";

/// The snail of the three rows.
fn snail(name: &str) -> Snail {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let t = catalog.tables.iter().find(|t| t.name == "s.t").unwrap();
    let empty = Rows {
        columns: t
            .columns
            .iter()
            .map(|c| warren_snail::values::Values::new(c.ty))
            .collect(),
    };
    write(&path, &catalog, &[("s.t".into(), empty)]).unwrap();
    let mut snail = Snail::open_writing(&path).unwrap();
    snail.execute(ROWS).unwrap();
    snail
}

/// The text of an expression over row `id`, `NULL`, or `ERROR: ` and the first line of the
/// refusal.
fn answer(snail: &mut Snail, expr: &str, id: i32) -> String {
    let q = format!(
        "WITH t AS (SELECT {COLUMNS} FROM s.t WHERE id = {id}) SELECT ({expr})::text AS x FROM t"
    );
    match snail.ask(&q, None) {
        Ok((b, _)) => {
            let a = b.column(0).as_string::<i32>();
            if a.is_null(0) {
                "NULL".into()
            } else {
                a.value(0).to_string()
            }
        }
        Err(e) => format!(
            "ERROR: {}",
            e.to_string().lines().next().unwrap_or_default()
        ),
    }
}

/// Each expression over each of the three rows, against PostgreSQL's answers.
fn check(name: &str, cases: &[(&str, [&str; 3])]) {
    let mut snail = snail(name);
    let mut wrong = Vec::new();
    for (expr, theirs) in cases {
        for (id, want) in (1..=3).zip(theirs) {
            let got = answer(&mut snail, expr, id);
            if got != *want {
                wrong.push(format!(
                    "{expr}, row {id}: {got}, where PostgreSQL answers {want}"
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn the_geometric_functions_answer_as_geo_ops_c() {
    check("rest-geo", &[
        ("area(pa)", ["6", "NULL", "NULL"]),
        ("area(b)", ["4", "0", "NULL"]),
        ("area(c)", ["12.566370614359172", "0", "NULL"]),
        ("width(b) + height(b)", ["4", "0", "NULL"]),
        ("center(b)", ["(2,3)", "(0,0)", "NULL"]),
        ("diagonal(b)", ["[(3,4),(1,2)]", "[(0,0),(0,0)]", "NULL"]),
        ("diameter(c)", ["4", "0", "NULL"]),
        ("radius(c)", ["2", "0", "NULL"]),
        ("isclosed(pa)", ["true", "false", "NULL"]),
        ("pclose(pa)", ["((0,0),(4,0),(4,3))", "((0,0),(3,4))", "NULL"]),
        ("popen(pa)", ["[(0,0),(4,0),(4,3)]", "[(0,0),(3,4)]", "NULL"]),
        ("npoints(pa)", ["3", "2", "NULL"]),
        ("length(sg)", ["5", "4", "NULL"]),
        ("length(pa)", ["12", "5", "NULL"]),
        ("isvertical(sg)", ["false", "true", "NULL"]),
        ("ishorizontal(p, q)", ["false", "true", "NULL"]),
        ("isparallel(sg, '[(0,0),(6,8)]')", ["true", "false", "NULL"]),
        ("slope(p, q)", ["1.3333333333333333", "Infinity", "NULL"]),
        ("box(p, q)", ["(4,6),(1,2)", "(1,2),(1,2)", "NULL"]),
        ("line(p, q)", ["{1.3333333333333333,-1,0.6666666666666667}", "ERROR: invalid line specification: must be two distinct points", "NULL"]),
        ("lseg(p, q)", ["[(1,2),(4,6)]", "[(1,2),(1,2)]", "NULL"]),
        ("circle(p, 2.5)", ["<(1,2),2.5>", "<(1,2),2.5>", "NULL"]),
        ("point(n, 1.5)", ["(7,1.5)", "(-1,1.5)", "(2147483647,1.5)"]),
        ("polygon(4, c)", ["((-1,1),(0.9999999999999999,3),(3,1.0000000000000002),(1.0000000000000004,-1))", "ERROR: cannot convert circle with radius zero to polygon", "NULL"]),
        ("polygon(n, '<(0,0),1>')", ["((-1,0),(-0.6234898018587336,0.7818314824680298),(0.22252093395631434,0.9749279121818236),(0.900968867902419,0.43388373911755823),(0.9009688679024191,-0.433883739117558),(0.2225209339563146,-0.9749279121818236),(-0.6234898018587334,-0.7818314824680299))", "ERROR: must request at least 2 points", "ERROR: too many points requested"]),
    ]);
}

#[test]
fn the_network_functions_answer_as_network_c_and_mac_c() {
    check(
        "rest-net",
        &[
            ("abbrev(i)", ["192.168.1.5/20", "2001:db8::1/64", "NULL"]),
            ("abbrev(ci)", ["10.1.0/17", "0:0:1::0/128", "NULL"]),
            (
                "broadcast(i)",
                [
                    "192.168.15.255/20",
                    "2001:db8::ffff:ffff:ffff:ffff/64",
                    "NULL",
                ],
            ),
            (
                "hostmask(i)",
                ["0.0.15.255/32", "::ffff:ffff:ffff:ffff/128", "NULL"],
            ),
            (
                "netmask(i)",
                ["255.255.240.0/32", "ffff:ffff:ffff:ffff::/128", "NULL"],
            ),
            ("network(i)", ["192.168.0.0/20", "2001:db8::/64", "NULL"]),
            ("host(i)", ["192.168.1.5", "2001:db8::1", "NULL"]),
            ("family(i)", ["4", "6", "NULL"]),
            ("masklen(ci)", ["17", "128", "NULL"]),
            (
                "set_masklen(i, 8)",
                ["192.168.1.5/8", "2001:db8::1/8", "NULL"],
            ),
            ("set_masklen(ci, 8)", ["10.0.0.0/8", "::/8", "NULL"]),
            (
                "set_masklen(i, n)",
                ["192.168.1.5/7", "2001:db8::1/128", "NULL"],
            ),
            (
                "trunc(m)",
                ["08:00:2b:00:00:00", "ff:ff:ff:00:00:00", "NULL"],
            ),
            (
                "trunc(m8)",
                ["08:00:2b:00:00:00:00:00", "ff:ff:ff:00:00:00:00:00", "NULL"],
            ),
            (
                "macaddr8_set7bit(m8)",
                ["0a:00:2b:01:02:03:04:05", "ff:ff:ff:ff:ff:ff:ff:ff", "NULL"],
            ),
        ],
    );
}

#[test]
fn the_bit_string_functions_answer_as_varbit_c() {
    check(
        "rest-bits",
        &[
            ("bit_count(a)", ["6", "8", "NULL"]),
            ("octet_length(a)", ["2", "2", "NULL"]),
            (
                "get_bit(a, n)",
                ["1", "ERROR: bit index -1 out of valid range (0..8)", "NULL"],
            ),
            ("set_bit(a, 3, 0)", ["1010001110", "111011110", "NULL"]),
            ("position(bb in a)", ["5", "8", "NULL"]),
            ("substring(a from 3 for 4)", ["1100", "1111", "NULL"]),
            ("substring(a, n)", ["1110", "111111110", "NULL"]),
            (
                "overlay(a placing bb from 2)",
                ["1001101110", "110111110", "NULL"],
            ),
            (
                "overlay(a placing bb from 2 for n)",
                ["1001110", "110111111110", "NULL"],
            ),
        ],
    );
}

#[test]
fn the_enum_functions_answer_as_enum_c_a_null_of_the_type_too() {
    check(
        "rest-enums",
        &[
            ("enum_first(e)", ["sad", "sad", "sad"]),
            ("enum_last(NULL::s.mood)", ["happy", "happy", "happy"]),
            (
                "enum_range(e)",
                ["{sad,ok,happy}", "{sad,ok,happy}", "{sad,ok,happy}"],
            ),
            ("enum_range(e, 'ok')", ["{ok}", "{}", "{sad,ok}"]),
            (
                "enum_range(NULL, e)",
                ["{sad,ok}", "{sad,ok,happy}", "{sad,ok,happy}"],
            ),
            ("enum_send(e)", ["\\x6f6b", "\\x6861707079", "NULL"]),
        ],
    );
}

#[test]
fn the_uuid_functions_answer_as_uuid_c() {
    check(
        "rest-uuid",
        &[
            ("uuid_extract_version(u)", ["7", "1", "NULL"]),
            (
                "uuid_extract_timestamp(u)",
                [
                    "2022-02-22 19:22:22+00",
                    "1998-02-02 20:23:12.902876+00",
                    "NULL",
                ],
            ),
            (
                "uuid_extract_version(uuidv7(interval '1 day'))",
                ["7", "7", "7"],
            ),
            (
                "uuid_extract_version('00000000-0000-7000-c000-000000000000'::uuid)",
                ["NULL", "NULL", "NULL"],
            ),
            (
                "uuid_extract_timestamp('ffffffff-ffff-ffff-bfff-ffffffffffff'::uuid)",
                ["NULL", "NULL", "NULL"],
            ),
            (
                "uuidv7(interval 'infinity')",
                [
                    "ERROR: interval out of range for UUID version 7",
                    "ERROR: interval out of range for UUID version 7",
                    "ERROR: interval out of range for UUID version 7",
                ],
            ),
        ],
    );
}

#[test]
fn the_fixed_settings_read_and_set_to_themselves() {
    check(
        "rest-settings",
        &[
            ("current_setting('timezone')", ["UTC", "UTC", "UTC"]),
            (
                "current_setting('DATESTYLE', true)",
                ["ISO, MDY", "ISO, MDY", "ISO, MDY"],
            ),
            (
                "set_config('extra_float_digits', ' +1 ', false)",
                ["1", "1", "1"],
            ),
        ],
    );
}

#[test]
fn the_rest_and_the_forms_a_call_resolves_to() {
    check(
        "rest-rest",
        &[
            (
                "format_type(o, n)",
                ["character varying(3)", "integer[]", "NULL"],
            ),
            (
                "format_type(1186, 2147418115)",
                ["interval(3)", "interval(3)", "interval(3)"],
            ),
            ("format_type(1042, -1)", ["bpchar", "bpchar", "bpchar"]),
            (
                "format_type(1700, 655366)",
                ["numeric(10,2)", "numeric(10,2)", "numeric(10,2)"],
            ),
            ("pg_typeof(i)", ["inet", "inet", "inet"]),
            ("pg_typeof('x')", ["unknown", "unknown", "unknown"]),
            ("pg_basetype(o)", ["character varying", "integer[]", "NULL"]),
            (
                "makeaclitem(99999, 99998, 'select, ALTER SYSTEM', true)",
                ["99999=r*A*/99998", "99999=r*A*/99998", "99999=r*A*/99998"],
            ),
            ("pg_snapshot_xmin(sn)", ["10", "1", "NULL"]),
            (
                "pg_visible_in_snapshot('12', sn)",
                ["false", "false", "NULL"],
            ),
            (
                "txid_snapshot_xmax(sn::text::txid_snapshot)",
                ["20", "1", "NULL"],
            ),
            ("tid_block(t)", ["7", "4294967295", "NULL"]),
            ("tid_offset(t)", ["9", "65535", "NULL"]),
            ("error_on_null(n)", ["7", "-1", "2147483647"]),
            (
                "error_on_null(o)",
                ["1043", "1007", "ERROR: null value not allowed"],
            ),
            (
                "area(NULL)",
                [
                    "ERROR: function area(unknown) is not unique",
                    "ERROR: function area(unknown) is not unique",
                    "ERROR: function area(unknown) is not unique",
                ],
            ),
            (
                "enum_first(NULL)",
                [
                    "ERROR: function enum_first(unknown) does not exist",
                    "ERROR: function enum_first(unknown) does not exist",
                    "ERROR: function enum_first(unknown) does not exist",
                ],
            ),
            (
                "error_on_null('x')",
                [
                    "ERROR: could not determine polymorphic type because input has type unknown",
                    "ERROR: could not determine polymorphic type because input has type unknown",
                    "ERROR: could not determine polymorphic type because input has type unknown",
                ],
            ),
        ],
    );
}

#[test]
fn a_setting_the_snail_does_not_hold_fixed_is_refused_by_name() {
    // work_mem changes no answer, so it is read at PostgreSQL's boot value; a setting missing, where
    // that is asked for, is NULL; TimeZone reset is the one the snail answers under.
    let mut snail = snail("rest-refused");
    assert_eq!(answer(&mut snail, "current_setting('work_mem')", 1), "4MB");
    assert_eq!(answer(&mut snail, "current_setting('no_such', true)", 1), "NULL");
    assert_eq!(answer(&mut snail, "set_config('TimeZone', NULL, false)", 1), "UTC");
    let got = answer(&mut snail, "set_config('TimeZone', 'Europe/Lisbon', false)", 1);
    assert!(got.contains("the snail answers under"), "{got}");
}
