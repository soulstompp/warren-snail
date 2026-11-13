//! A snail of format 7 or 8, as warren-snail 0.1.0 or 0.1.1 made it, is refused by name as it is
//! opened, to read or to write, by the library and by the command, with the way across: `out` it
//! with 0.1.x and `write` it afresh with this build. Each file in `tests/formats` was made by the
//! version published on crates.io: `write` of `DDL` with no rows, then `exec` of
//! `INSERT INTO t VALUES (1, 'one'), (2, 'two')`, into `seven.snail` by 0.1.0 and `eight.snail` by
//! 0.1.1; and `out.parquet`, the table each version put `out` of its own snail, the two byte for
//! byte the same.

mod weather;

use std::path::{Path, PathBuf};
use std::process::Command;
use warren_snail::read::Snail;
use weather::tmp;

#[cfg(feature = "standalone")]
const DDL: &str = "CREATE TABLE t (id bigint PRIMARY KEY, name text);";

/// The way across, after the format a file names.
const WAY: &str = "made by 0.1.x: `out` it with 0.1.x and `write` it afresh with 0.2.0";

/// The old snails: the file, the version that made it and the format it names.
const MADE: [(&str, &str, u32); 2] = [("seven.snail", "0.1.0", 7), ("eight.snail", "0.1.1", 8)];

fn made(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/formats")
        .join(name)
}

/// A copy of a file `made` names, where a test may write.
fn copied(name: &str) -> PathBuf {
    let path = tmp(&format!("formats-{name}"));
    std::fs::copy(made(name), &path).unwrap();
    path
}

#[test]
fn a_snail_0_1_x_made_is_refused_by_name_as_it_is_opened_and_left_as_it_was() {
    for (name, version, format) in MADE {
        let path = copied(name);
        let before = std::fs::read(&path).unwrap();
        assert_eq!(before[8..12], format.to_le_bytes(), "{version}");
        let says = format!("the snail is of format {format}, {WAY}");
        assert_eq!(
            Snail::open(&path).err().unwrap().to_string(),
            says,
            "{version}"
        );
        assert_eq!(
            warren_snail::file::Reader::open(&path)
                .unwrap()
                .root()
                .unwrap_err()
                .to_string(),
            says,
            "{version}"
        );
        assert_eq!(
            warren_snail::file::Writer::open(&path)
                .err()
                .unwrap()
                .to_string(),
            says,
            "{version}"
        );
        #[cfg(feature = "standalone")]
        assert_eq!(
            Snail::open_writing(&path).err().unwrap().to_string(),
            says,
            "{version}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "{version}");
    }
}

#[test]
fn the_command_refuses_a_snail_0_1_x_made_by_name() {
    for (name, version, format) in MADE {
        let path = copied(name);
        let dir = tmp(&format!("formats-{version}-out"));
        let says = format!("the snail is of format {format}, {WAY}\n");
        for args in [
            vec!["census", path.to_str().unwrap()],
            vec![
                "out",
                path.to_str().unwrap(),
                "--dir",
                dir.to_str().unwrap(),
            ],
        ] {
            let run = Command::new(env!("CARGO_BIN_EXE_warren-snail"))
                .args(&args)
                .output()
                .unwrap();
            assert!(!run.status.success(), "{version} {args:?}");
            assert_eq!(
                String::from_utf8_lossy(&run.stderr),
                says,
                "{version} {args:?}"
            );
        }
    }
}

#[test]
#[cfg(feature = "standalone")]
fn what_0_1_x_puts_out_is_written_afresh_its_rows_whole() {
    use weather::{fresh, ints64, texts};
    let path = tmp("formats-afresh.snail");
    warren_snail::write::write_parquet(
        &path,
        &warren_snail::catalog::Catalog::parse(DDL).unwrap(),
        &[("t".into(), made("out.parquet"))],
        weather::span(),
        None,
        None,
    )
    .unwrap();
    // Its root, of generation 1, lies in the second copy, and names format 9.
    assert_eq!(
        std::fs::read(&path).unwrap()[4096 + 8..4096 + 12],
        9u32.to_le_bytes()
    );
    let (b, _) = fresh(&path)
        .ask("SELECT id, name FROM t ORDER BY id", None)
        .unwrap();
    assert_eq!(ints64(b.column(0).as_ref()), [Some(1), Some(2)]);
    assert_eq!(
        texts(b.column(1).as_ref()),
        [Some("one".to_string()), Some("two".to_string())]
    );
}
