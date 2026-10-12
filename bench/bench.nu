# The snail's benchmark: each question of bench/questions/ asked of PostgreSQL and of the snail, on the
# whole of a database sqlc-brickgen built, its answer certified before any time is taken.
#
#   nu bench/bench.nu postgres://USER@HOST:5432/lego [--runs 5] [--seed]
#
# PostgreSQL is the database as it stands, read only: the bench records its indexes on the two
# tables, its settings and its version. The snail is bench/out/<database>/<database>.snail, written
# by load.nu.
#
# For each question, in order:
# - the certificate: the snail's answer digested against the truth, the question's file run twice on
#   PostgreSQL (certificate.nu); RIGHT or WRONG;
# - a probe: the snail once, which sets PostgreSQL's cap at ten times its time, at least a second;
# - `--runs` repetitions of each, the first side alternating by question. Each PostgreSQL repetition
#   is a fresh session running EXPLAIN (ANALYZE, BUFFERS, TIMING OFF, FORMAT JSON): session-cold,
#   never buffer-cold. Each snail repetition is a fresh process asking the question twice on one open
#   snail: `snail`, the first round, timed from opening the file to the answer; `snail held`, the
#   second, timed from its own start, finding held what the first kept (128 MB unless the snail is
#   told otherwise, PostgreSQL's default shared_buffers).
#
# Refuses to start while another client session is on the server. Writes
# bench/out/<database>/<UTC time>/: certificate.parquet, runs.parquet, machine.parquet,
# settings.parquet and summary.md. Exits 1 on any WRONG answer. --seed drops one row of every snail
# answer before its digest, which must make every answer WRONG.

plugin use polars
use digest.nu *
use common.nu *
use certificate.nu *

const PG_PAGE = 8192
const SNAIL_PAGE = 4096

# The load average and the other client sessions, now.
def machine [url: string, when: string] {
    let load = (open /proc/loadavg | split row " " | first 3 | str join " ")
    let sessions = (one-value $url "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend' AND pid <> pg_backend_pid()" | into int)
    {when: $when, load: $load, other_sessions: $sessions}
}

def pg-rep [url: string, sql: string, cap_ms: int] {
    let r = (psql-in $url $"SET statement_timeout = '($cap_ms)ms'" $"EXPLAIN \(ANALYZE, BUFFERS, TIMING OFF, FORMAT JSON\) ($sql)")
    if $r.exit_code != 0 {
        let outcome = if ($r.stderr | str contains "statement timeout") { "timeout" } else { "error" }
        return {outcome: $outcome, ms: null, planning_ms: null, shared_hit: null, shared_read: null, temp_read: null,
                temp_written: null, planning_hit: null, planning_read: null, note: ($r.stderr | str trim | str substring 0..<300)}
    }
    let j = ($r.stdout | from json | first)
    let plan = $j.Plan
    let planning = ($j | get -o Planning | default {})
    {outcome: "ok", ms: ($j."Execution Time" | into float), planning_ms: ($j."Planning Time" | into float),
     shared_hit: ($plan."Shared Hit Blocks" | into int), shared_read: ($plan."Shared Read Blocks" | into int),
     temp_read: ($plan."Temp Read Blocks" | into int), temp_written: ($plan."Temp Written Blocks" | into int),
     planning_hit: ($planning | get -o "Shared Hit Blocks" | default 0 | into int),
     planning_read: ($planning | get -o "Shared Read Blocks" | default 0 | into int), note: null}
}

# One snail repetition, a fresh process: its two rounds, `snail` and `snail held`. A round's pages are
# those read from the file; `pages_held` those it found held.
def snail-rep [file: path, question: record, tmp: path] {
    let answer = ($tmp | path join $"($question.question).parquet")
    let round = ($tmp | path join $"($question.question)_round.parquet")
    let r = (^(snail-bin) read $file $question.file --out $answer --round $round --rounds 2 | complete)
    if $r.exit_code != 0 {
        let note = ($r.stderr | str trim | str substring 0..<300)
        return ([snail "snail held"] | each {|t|
            {target: $t, outcome: "error", ms: null, pages: null, pages_held: null, directory_pages: null, column_pages: null,
             column_pages_held: null, predicted_is_read: null, rchar_bytes: null, held_bytes: null, answer: null, note: $note}
        })
    }
    polars open $round | polars into-nu | each {|x|
        {target: (if $x.round_no == 1 { "snail" } else { "snail held" }), outcome: "ok", ms: ($x.elapsed_us / 1000.0),
         pages: $x.pages_counted, pages_held: $x.pages_held, directory_pages: $x.directory_pages,
         column_pages: $x.column_pages_read, column_pages_held: $x.column_pages_held,
         predicted_is_read: ($x.column_pages_predicted - $x.column_pages_held == $x.column_pages_read),
         rchar_bytes: $x.rchar_bytes, held_bytes: $x.held_bytes, answer: $answer, note: null}
    }
}

# A spread as text: median ± MAD [min..max], or a dash where no RIGHT repetition finished.
def shown [s: record] {
    if $s.median == null { return "—" }
    let r = {|x| $x | math round --precision 2 }
    $"(do $r $s.median) ± (do $r $s.mad) [(do $r $s.min)..(do $r $s.max)]"
}

# The median and the median absolute deviation, with the extremes.
def spread [xs: list] {
    if ($xs | is-empty) { return {median: null, mad: null, min: null, max: null} }
    let m = ($xs | math median)
    {median: $m, mad: ($xs | each {|x| ($x - $m) | math abs } | math median), min: ($xs | math min), max: ($xs | math max)}
}

def main [url: string, --runs: int = 5, --seed] {
    let db = (db-name $url)
    digest-self-check
    let file = (snail-file $url)
    if not ($file | path exists) { error make {msg: $"($file) is missing: run bench/load.nu ($url) first"} }
    let before = (machine $url "before")
    if $before.other_sessions > 0 {
        error make {msg: $"($before.other_sessions) other client sessions are on the server; not starting"}
    }
    let stamp = (date now | date to-timezone UTC | format date "%Y%m%dT%H%M%SZ")
    let dir = (out-dir | path join $db $stamp)
    let tmp = ($dir | path join tmp)
    mkdir $tmp
    let questions = (questions)

    let settings = ([
        {key: "server_version", value: (one-value $url "SHOW server_version")}
        {key: "shared_buffers", value: (one-value $url "SHOW shared_buffers")}
        {key: "session_preload_libraries", value: (one-value $url "SHOW session_preload_libraries")}
        {key: "purchases_rows", value: (one-value $url "SELECT count(*) FROM lego.lego_purchases")}
        {key: "collection_rows", value: (one-value $url "SELECT count(*) FROM lego.lego_collection")}
        {key: "purchases_bytes_with_indexes", value: (one-value $url "SELECT pg_total_relation_size('lego.lego_purchases')")}
        {key: "collection_bytes_with_indexes", value: (one-value $url "SELECT pg_total_relation_size('lego.lego_collection')")}
        {key: "indexes", value: (one-value $url "SELECT string_agg(indexrelid::regclass::text, ', ' ORDER BY 1) FROM pg_index WHERE indrelid IN ('lego.lego_purchases'::regclass, 'lego.lego_collection'::regclass)")}
        {key: "snail_bytes", value: (ls $file | get size.0 | into int | into string)}
        {key: "runs", value: ($runs | into string)}
    ])

    mut certificates = []
    mut rows = []
    mut held_bytes = 0
    for q in ($questions | enumerate) {
        let question = $q.item
        let probe = (snail-rep $file $question $tmp)
        let first = ($probe | first)
        if $held_bytes == 0 and $first.held_bytes != null { $held_bytes = $first.held_bytes }
        let cap = ([1000 (if $first.ms == null { 1000 } else { ($first.ms * 10 | math ceil) })] | math max)
        let standing = if $first.outcome != "ok" { "ERROR" } else {
            let schema = (polars open $first.answer | polars schema)
            let truth = (truth $url $question $schema $tmp)
            let frame = (polars open $first.answer | polars select ...(as-text $schema))
            let frame = if $seed { $frame | polars slice 1 1000000000 } else { $frame }
            let d = ($frame | digest-of ($schema | columns))
            $certificates = ($certificates | append {question: $question.question, rows: $d.rows, digest: $d.digest,
                              truth_rows: $truth.rows, truth_digest: $truth.digest})
            if $d.digest == $truth.digest and $d.rows == $truth.rows { "RIGHT" } else { "WRONG" }
        }
        $rows = ($rows | append ($probe | each {|p| {question: $question.question, rep: 0, cap_ms: null, standing: $standing} | merge $p | reject answer held_bytes }))
        for rep in 1..$runs {
            let pg_first = (($q.index + $rep) mod 2) == 0
            let sides = if $pg_first { ["postgres" "snail"] } else { ["snail" "postgres"] }
            for side in $sides {
                if $side == "postgres" {
                    let sql = (open --raw $question.file | str trim | str trim --right --char ';')
                    $rows = ($rows | append ({question: $question.question, target: "postgres", rep: $rep, cap_ms: $cap, standing: $standing} | merge (pg-rep $url $sql $cap)))
                } else {
                    $rows = ($rows | append (snail-rep $file $question $tmp | each {|p| {question: $question.question, rep: $rep, cap_ms: null, standing: $standing} | merge $p | reject answer held_bytes }))
                }
            }
        }
        print $"($question.question): ($standing)"
    }
    let after = (machine $url "after")
    rm -rf $tmp

    $certificates | polars into-df --schema {question: str, rows: i64, digest: str, truth_rows: i64, truth_digest: str} | polars save ($dir | path join certificate.parquet)
    [$before $after] | polars into-df --schema {when: str, load: str, other_sessions: i64} | polars save ($dir | path join machine.parquet)
    $settings | append {key: "snail_held_bytes", value: ($held_bytes | into string)} | polars into-df --schema {key: str, value: str} | polars save ($dir | path join settings.parquet)
    let all = ($rows | each {|r| {
        question: $r.question, target: $r.target, rep: $r.rep, standing: $r.standing, outcome: $r.outcome,
        ms: $r.ms, cap_ms: $r.cap_ms, note: $r.note,
        planning_ms: ($r | get -o planning_ms), shared_hit: ($r | get -o shared_hit), shared_read: ($r | get -o shared_read),
        temp_read: ($r | get -o temp_read), temp_written: ($r | get -o temp_written),
        planning_hit: ($r | get -o planning_hit), planning_read: ($r | get -o planning_read),
        pages: ($r | get -o pages), pages_held: ($r | get -o pages_held), directory_pages: ($r | get -o directory_pages),
        column_pages: ($r | get -o column_pages), column_pages_held: ($r | get -o column_pages_held),
        predicted_is_read: ($r | get -o predicted_is_read), rchar_bytes: ($r | get -o rchar_bytes)} })
    $all | polars into-df --schema {question: str, target: str, rep: i64, standing: str, outcome: str, ms: f64, cap_ms: i64, note: str,
        planning_ms: f64, shared_hit: i64, shared_read: i64, temp_read: i64, temp_written: i64, planning_hit: i64, planning_read: i64,
        pages: i64, pages_held: i64, directory_pages: i64, column_pages: i64, column_pages_held: i64, predicted_is_read: bool, rchar_bytes: i64}
    | polars save ($dir | path join runs.parquet)

    # The summary: pages first, then time, over the repetitions (the probe left out), RIGHT answers only.
    let lines = ($questions | each {|question|
        let reps = ($all | where {|r| $r.question == $question.question and $r.rep > 0 })
        let pg = ($reps | where target == postgres)
        let sn = ($reps | where target == snail)
        let sh = ($reps | where target == "snail held")
        let ok = {|xs| $xs | where outcome == ok | where standing == RIGHT }
        let pg_ms = (do $ok $pg | each {|r| $r.planning_ms + $r.ms })
        let sn_ms = (do $ok $sn | get ms)
        let sh_ms = (do $ok $sh | get ms)
        let p = (spread $pg_ms)
        let s = (spread $sn_ms)
        let h = (spread $sh_ms)
        let standing_of = {|xs, x| if ($pg_ms | is-empty) or ($xs | is-empty) { "no standing" } else if $x.max < $p.min { "clear win" } else if $p.max < $x.min { "clear loss" } else { "overlap" } }
        let standing = (do $standing_of $sn_ms $s)
        let pg0 = (do $ok $pg | get -o 0 | default {})
        let sn0 = (do $ok $sn | get -o 0 | default {})
        let sh0 = (do $ok $sh | get -o 0 | default {})
        let not_ok = ($reps | where outcome != ok | length)
        {question: $question.question, answer: ($reps | get -o 0.standing | default "?"),
         pg_hit: ($pg0 | get -o shared_hit), pg_read: ($pg0 | get -o shared_read), pg_temp_written: ($pg0 | get -o temp_written),
         pg_bytes: (if ($pg0 | is-empty) { null } else { ($pg0.shared_hit + $pg0.shared_read) * $PG_PAGE }),
         snail_pages: ($sn0 | get -o pages), snail_bytes: (if ($sn0 | is-empty) { null } else { $sn0.pages * $SNAIL_PAGE }),
         held_read: ($sh0 | get -o pages), held_found: ($sh0 | get -o pages_held),
         pg_ms: (shown $p), snail_ms: (shown $s), held_ms: (shown $h),
         standing: $standing, held_standing: (do $standing_of $sh_ms $h), not_ok: $not_ok}
    })
    let md = ([
        $"# The snail against PostgreSQL on ($db), ($stamp)"
        ""
        "Pages first: PostgreSQL's 8 KB pages hit in shared buffers and read from outside them, and temp pages written; the snail's 4 KB pages read through `pread` (`snail_pages` on its first round, `held_read` on its second) or found held (`held_found`). Then time, in ms, PostgreSQL's planning and execution against the snail's first round from opening the file (`snail_ms`) and its second round on the snail held open (`held_ms`): median ± MAD [min..max] over the repetitions, RIGHT answers only; the standing reads each side at its unfavourable corner."
        ""
        ($lines | to md)
        ""
        $"the snail held up to ($held_bytes) bytes between rounds"
        ""
        $"machine before: ($before.load), ($before.other_sessions) other sessions; after: ($after.load), ($after.other_sessions) other sessions"
    ] | str join "\n")
    $md | save --force ($dir | path join summary.md)
    print $md
    print $"written to ($dir)"
    if ($certificates | any {|c| $c.digest != $c.truth_digest or $c.rows != $c.truth_rows }) { exit 1 }
}
