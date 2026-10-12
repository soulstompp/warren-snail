# Writes the purchases and the collection of a database sqlc-brickgen built into a snail: both tables
# whole, read only, as typed Parquet, then `warren-snail write` with `lego.sql`. Then checks that the
# snail holds each table as PostgreSQL does: its rows, and each column's NULLs and text bytes.
#
#   nu bench/load.nu postgres://USER@HOST:5432/lego
#
# The database must hold every index `lego.sql` names, which sqlc-brickgen builds by default and
# with `--indexes keys,composite`. Writes bench/out/<database>/: the two tables as Parquet, the snail
# <database>.snail, and blocks.parquet, what each order laid out. PostgreSQL is only read. --seed drops
# one row of the collection before the snail is written, which the check must refuse.

plugin use polars
use common.nu *

const TABLES = [lego_purchases lego_collection]

def main [url: string, --seed] {
    let bin = (snail-bin)
    if not ($bin | path exists) { error make {msg: $"($bin) is missing: run `cargo build --release` in the workspace"} }
    present $url
    let dir = (out-dir | path join (db-name $url))
    mkdir $dir
    let micros_of = {|c| polars col $c | polars cast 'datetime<us, UTC>' }

    let csv = ($dir | path join purchases.csv)
    copy-csv $url $"SELECT purchase_id, builder_id, row_no, store, (micros ordered_at) AS ordered_at, ordered_local, (micros delivered_at) AS delivered_at FROM lego.lego_purchases ORDER BY purchase_id" $csv
    polars open $csv --schema {purchase_id: i64, builder_id: i32, row_no: i32, store: str, ordered_at: i64, ordered_local: str, delivered_at: i64}
    | polars select (polars col purchase_id) (polars col builder_id) (polars col row_no) (polars col store) (do $micros_of ordered_at) (polars col ordered_local) (do $micros_of delivered_at)
    | polars save ($dir | path join purchases.parquet)
    rm $csv

    let csv = ($dir | path join collection.csv)
    copy-csv $url "SELECT builder_id, row_no, set_num, typed_set_num, typed_name FROM lego.lego_collection ORDER BY builder_id, row_no" $csv
    let collection = (polars open $csv --schema {builder_id: i32, row_no: i32, set_num: str, typed_set_num: str, typed_name: str})
    let collection = if $seed { $collection | polars slice 1 1000000000 } else { $collection }
    $collection | polars save ($dir | path join collection.parquet)
    rm $csv

    let file = (snail-file $url)
    for f in (glob $"($file)*") { rm $f }
    ^$bin write $file --ddl (ddl) $"lego.lego_purchases=($dir | path join purchases.parquet)" $"lego.lego_collection=($dir | path join collection.parquet)"
    let blocks = ($dir | path join blocks.parquet)
    ^$bin blocks $file --out $blocks
    census $url $blocks
}

# Stops unless the database holds the clock and every index `lego.sql` names, the primary keys among
# them.
def present [url: string] {
    let named = (open --raw (ddl) | parse --regex 'CREATE INDEX (?<index>\w+) ON lego\.(?<table>\w+)' | get index
        | append ($TABLES | each {|t| $"($t)_pkey" }))
    let r = (psql-in $url $"SELECT indexname FROM pg_indexes WHERE schemaname = 'lego' AND tablename IN \(($TABLES | each {|t| $"'($t)'" } | str join ', ')\)")
    if $r.exit_code != 0 { error make {msg: $"psql: ($r.stderr)"} }
    let missing = ($named | where {|i| $i not-in ($r.stdout | lines) })
    if ($missing | is-not-empty) {
        error make {msg: $"the database lacks ($missing | str join ', '): build it with sqlc-brickgen's default --indexes, or keys,composite"}
    }
    if (one-value $url "SELECT to_regprocedure('lego.clock(timestamptz)') IS NOT NULL") != "t" {
        error make {msg: "the database lacks lego.clock(timestamptz), which sqlc-brickgen creates"}
    }
}

# Stops unless each table's own order holds the rows PostgreSQL's table does, and each column the
# NULLs and the text bytes PostgreSQL counts.
def census [url: string, blocks: path] {
    let all = (polars open $blocks | polars into-nu)
    let rows = ($TABLES | each {|t|
        let table = $"lego.($t)"
        let mine = ($all | where table == $table)
        # The table's own order is the one that holds every column.
        let order = ($mine | group-by order | items {|o, rs| {order: $o, columns: ($rs | get column | uniq | length)} }
            | sort-by columns | last | get order)
        let along = ($mine | where order == $order)
        let columns = ($along | get column | uniq)
        let texts = ($columns | where {|c| ($along | where column == $c | get bytes | compact | is-not-empty) })
        let sql = (["count(*)"] | append ($columns | each {|c| $"count\(*\) - count\(($c)\)" })
            | append ($texts | each {|c| $"coalesce\(sum\(octet_length\(($c)\)\), 0\)" }) | str join ", ")
        let pg = (one-value $url $"SELECT ($sql) FROM ($table)" | split row "|" | into int)
        let first = ($columns | first)
        let snail_rows = ($along | where column == $first | get rows | math sum)
        [{table: $t, column: "", what: rows, postgres: $pg.0, snail: $snail_rows}]
        | append ($columns | enumerate | each {|c|
            {table: $t, column: $c.item, what: nulls, postgres: ($pg | get ($c.index + 1)),
             snail: ($along | where column == $c.item | get nulls | math sum)}
        })
        | append ($texts | enumerate | each {|c|
            {table: $t, column: $c.item, what: bytes, postgres: ($pg | get ($c.index + 1 + ($columns | length))),
             snail: ($along | where column == $c.item | get bytes | math sum)}
        })
    } | flatten)
    $rows | table -e | print
    let wrong = ($rows | where {|r| $r.postgres != $r.snail })
    if ($wrong | is-not-empty) { error make {msg: $"the snail does not hold the tables as PostgreSQL does: ($wrong | to nuon)"} }
    print "the snail holds both tables as PostgreSQL does"
}
