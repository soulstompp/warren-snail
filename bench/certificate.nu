# The certificate of an answer: its rows as text, digested with `digest.nu`, and the truth it is held
# to, the question's own file run twice on PostgreSQL, read only. It has no main. Load the polars
# plugin in the script before `use certificate.nu *`.

use digest.nu *
use common.nu *

# An answer's columns as text, by its schema: an instant as its microseconds.
export def as-text [schema: record] {
    $schema | transpose name dtype | each {|c|
        if ($c.dtype | str starts-with datetime) {
            polars col $c.name | polars cast i64 | polars cast str | polars as $c.name
        } else {
            polars col $c.name | polars cast str | polars as $c.name
        }
    }
}

# A frame's rows as a count and a digest over the columns named.
export def digest-of [cols: list<string>] {
    $in
    | polars with-column (polars lit 1 | polars as mult) (polars lit x | polars as g)
    | row-hashes $cols | digest-sums [g] | digest-hex | polars collect | polars into-nu
    | get -o 0 | default {rows: 0, digest: "empty"}
}

# The question's file on PostgreSQL, every column as text the way `as-text` writes it, run twice; the
# two runs must agree.
export def truth [url: string, question: record, schema: record, tmp: path] {
    let sql = (open --raw $question.file | str trim | str trim --right --char ';')
    let cols = ($schema | transpose name dtype | each {|c|
        let q = $"q.\"($c.name)\""
        if ($c.dtype | str starts-with datetime) {
            $"\((micros $q)\)::text AS \"($c.name)\""
        } else {
            $"($q)::text AS \"($c.name)\""
        }
    } | str join ", ")
    let names = ($schema | columns)
    let runs = (1..2 | each {|i|
        let f = ($tmp | path join $"truth_($question.question)_($i).csv")
        copy-csv $url $"SELECT ($cols) FROM \(\n($sql)\n\) q" $f
        let d = (polars open $f --schema ($names | each {|n| {$n: str} } | into record) | digest-of $names)
        rm $f
        $d
    })
    if ($runs.0.digest != $runs.1.digest) or ($runs.0.rows != $runs.1.rows) {
        error make {msg: $"the truth for ($question.question) differs between its two runs"}
    }
    $runs.0
}
