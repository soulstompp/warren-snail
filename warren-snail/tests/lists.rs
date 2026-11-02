//! The lists of types, operators and operator classes PostgreSQL defines from 14 to 19, each object
//! held or refused by name as the snail's own paths answer it: a type by a table's column of it, an
//! operator class by an index of it, an operator by a pipeline's step over columns of its types.

#[cfg(feature = "standalone")]
mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::lists::{all, opclasses, types, Kind, Probed};

/// The census's count of each list's objects on 14 to 19 (FINDINGS-28.md): every type, every
/// operator, every operator class.
const COUNTS: [(Kind, [usize; 6]); 3] = [
    (Kind::Type, [453, 463, 465, 469, 473, 489]),
    (Kind::Operator, [804, 799, 799, 799, 799, 805]),
    (Kind::Opclass, [177, 177, 177, 177, 177, 179]),
];

fn named<'a>(probed: &'a [Probed], name: &str) -> &'a Probed {
    probed
        .iter()
        .find(|p| p.listed.name == name)
        .unwrap_or_else(|| panic!("{name} is not listed"))
}

/// Why the DDL reader refuses a probe, as the snail's own DDL path gives it.
fn refusal_of(ddl: &str) -> Option<String> {
    Catalog::read(ddl)
        .unwrap()
        .statements
        .into_iter()
        .find_map(|s| s.refused)
}

#[test]
fn each_list_names_each_release_s_objects_by_its_census_count() {
    for (kind, counts) in COUNTS {
        for (release, count) in (14..=19).zip(counts) {
            assert_eq!(
                all(kind).iter().filter(|o| o.in_release(release)).count(),
                count,
                "{kind:?} of {release}"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for o in all(kind) {
            assert!(seen.insert(o.name), "{} is listed twice", o.name);
        }
    }
    assert!(types(18).iter().all(|p| p.listed.name != "oid8"));
    assert!(types(19).iter().any(|p| p.listed.name == "oid8"));
    let (op, left, right) = named_listed(Kind::Operator, "-(NONE,integer)").operands();
    assert_eq!((op, left, right), ("-", None, "integer"));
}

fn named_listed(kind: Kind, name: &str) -> warren_snail::lists::Listed {
    *all(kind)
        .iter()
        .find(|o| o.name == name)
        .unwrap_or_else(|| panic!("{name} is not listed"))
}

#[test]
fn a_type_is_held_where_a_table_s_column_of_it_is_taken() {
    let of = types(19);
    for p in &of {
        assert_eq!(p.refused, refusal_of(&p.probe), "{}", p.listed.name);
        assert!(p.probe.contains(&format!(" c {});", p.listed.name)));
    }
    assert_eq!(named(&of, "integer").refused, None);
    assert_eq!(named(&of, "timestamp with time zone[]").refused, None);
    assert_eq!(
        named(&of, "pg_attribute").refused.as_deref(),
        Some("census.t.c: column \"attmissingval\" has pseudo-type anyarray")
    );
    assert!(named(&of, "anyelement").refused.is_some());
    assert_eq!(named(&of, "integer").listed.family, "numeric types");
    assert_eq!(named(&of, "\"char\"").listed.family, "internal-use types");
}

#[test]
fn an_operator_class_is_held_where_an_index_of_it_is_taken() {
    let of = opclasses(19);
    for p in &of {
        assert_eq!(p.refused, refusal_of(&p.probe), "{}", p.listed.name);
    }
    for held in [
        "int4_ops USING btree",
        "int4_ops USING hash",
        "range_ops USING gist",
        "enum_ops USING btree",
        "record_ops USING btree",
        "array_ops USING hash",
    ] {
        assert_eq!(named(&of, held).refused, None, "{held}");
    }
    // A pseudo-type's key is probed by a type it stands for, the enum and the composite declared.
    assert!(named(&of, "array_ops USING hash")
        .probe
        .contains(" c integer[]);"));
    assert!(named(&of, "enum_ops USING btree")
        .probe
        .starts_with("CREATE TYPE census.e AS ENUM ('a', 'b');"));
    assert_eq!(
        named(&of, "oidvector_ops USING btree").refused.as_deref(),
        None
    );
    assert_eq!(
        named_listed(Kind::Opclass, "int4_ops USING btree").input,
        "integer"
    );
    assert_eq!(named(&of, "int4_ops USING btree").listed.family, "btree");
}

#[test]
#[cfg(feature = "standalone")]
fn an_operator_is_held_where_a_pipeline_s_step_answers_it_over_columns_of_its_types() {
    let path = weather::tmp("lists-operators.snail");
    let (of, setup) = warren_snail::lists::operators(19, &path).unwrap();
    assert_eq!(of.len(), 805);
    for held in [
        "+(integer,integer)",
        "-(NONE,integer)",
        "<(anyenum,anyenum)",
        "=(record,record)",
        "@>(anyrange,anyelement)",
        "||(anycompatiblearray,anycompatible)",
    ] {
        assert_eq!(named(&of, held).refused, None, "{held}");
    }
    assert_eq!(
        named(&of, "-(NONE,integer)").probe,
        "WITH s AS (SELECT v_integer FROM census.t) SELECT - v_integer AS x FROM s"
    );
    // A step that refuses it refuses it in its words, here PostgreSQL's own.
    assert_eq!(
        named(&of, "+(aclitem[],aclitem)").refused.as_deref(),
        Some("aclinsert is no longer supported")
    );
    assert_eq!(named(&of, "=(oidvector,oidvector)").refused, None);
    // The setup makes the table whole for PostgreSQL: a column and a value of every operand type.
    assert!(setup.iter().any(|s| s.contains(" v_oidvector oidvector")));
    assert!(setup.contains(&"UPDATE census.t SET v_oidvector = '1 2' WHERE k = 1;".to_string()));
    // Asked again of a snail, each held operator's probe answers.
    let mut snail = warren_snail::read::Snail::open(&path).unwrap();
    let p = named(&of, "<(anyenum,anyenum)");
    let (b, _) = snail.ask(&p.probe, None).unwrap();
    assert_eq!(b.num_rows(), 1);
}
