# Shared by the bench's scripts. Load the polars plugin in the script before `use common.nu *`.

const HERE = path self .

# The bench's output directory, which git ignores.
export def out-dir [] { $HERE | path join out }

# The snail's DDL for the two tables.
export def ddl [] { $HERE | path join lego.sql }

# The snail's command: the workspace's release build.
export def snail-bin [] { $HERE | path join ../target/release/warren-snail | path expand }

# The snail a load wrote for a database, by the database's name.
export def snail-file [url: string] { out-dir | path join (db-name $url) $"(db-name $url).snail" }

# A database's name, the last part of its URL's path.
export def db-name [url: string] { $url | url parse | get path | str trim --left --char '/' }

# The questions, by name, and their files.
export def questions [] {
    glob ($HERE | path join questions '*.sql') | sort | each {|f| {question: ($f | path parse | get stem), file: $f} }
}

# An instant as microseconds since 1970-01-01 UTC in SQL, its infinities as the largest and smallest
# bigint, as the snail holds them.
export def micros [col: string] {
    $"CASE WHEN ($col) = 'infinity' THEN 9223372036854775807 WHEN ($col) = '-infinity' THEN -9223372036854775808 ELSE \(extract\(epoch FROM ($col)\) * 1000000\)::bigint END"
}

# psql on the database the URL names, read only, its sessions named warren-snail-bench.
export def psql-in [url: string, ...cmds: string] {
    let args = ($cmds | each {|c| [-c $c] } | flatten)
    with-env {PGAPPNAME: "warren-snail-bench", PGOPTIONS: "-c default_transaction_read_only=on"} {
        ^psql -d $url -X -q -At -v ON_ERROR_STOP=1 ...$args | complete
    }
}

# One value from the database.
export def one-value [url: string, sql: string] {
    let r = (psql-in $url $sql)
    if $r.exit_code != 0 { error make {msg: $"psql: ($r.stderr)"} }
    $r.stdout | str trim
}

# COPY (query) TO STDOUT as CSV with a header, into a file, read only.
export def copy-csv [url: string, query: string, file: path] {
    let r = (with-env {PGAPPNAME: "warren-snail-bench", PGOPTIONS: "-c default_transaction_read_only=on"} {
        ^psql -d $url -X -q -v ON_ERROR_STOP=1 -c "SET statement_timeout = '600s'" -c $"COPY \(($query)\) TO STDOUT WITH \(FORMAT csv, HEADER\)" | complete
    })
    if $r.exit_code != 0 { error make {msg: $"psql failed: ($r.stderr | str substring 0..<2000)"} }
    $r.stdout | save --force $file
}
