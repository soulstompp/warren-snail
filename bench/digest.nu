# The answer digest of warren-bench, the warren-surveyor workspace's bench, in nu and polars, sharing
# no code with it. A row is its column texts in result order, null for NULL.
#
#   use digest.nu *     (from a script in this directory, after it has loaded the polars plugin)
#
# A row is encoded as bytes, column by column: 0 for NULL; otherwise 1, the text's UTF-8 length as
# four bytes little-endian, then the text's UTF-8 bytes. The row gets two 64-bit hashes, FNV-1a from
# two offsets, each finished by the splitmix64 mixer. A bag of rows digests to its row count and the
# two sums of its rows' hashes modulo 2^64, each row counted with its multiplicity.
#
# The hashes are computed column-wise over all rows at once, one byte position at a time, in polars
# UInt64 arithmetic (which wraps modulo 2^64). XOR is only available elementwise in polars SQL, so
# each XOR step goes through `polars query`. A text's bytes are read one character at a time: every
# non-ASCII character is first rewritten as one private-use character per UTF-8 byte
# (U+E080..U+E0FF), so that character k is byte k.

const PRIME = 1099511628211        # 0x00000100000001B3
const OFFSET_A = "CBF29CE484222325"
const OFFSET_B = "6C62272E07BB0142"
const MIX_1 = "BF58476D1CE4E5B9"
const MIX_2 = "94D049BB133111EB"
const PRIVATE_BASE = 57344         # U+E000; bytes 128..255 map to U+E080..U+E0FF

# A 64-bit constant, from 16 hex digits, as a UInt64 literal expression.
export def u64-lit [hex: string] {
    let hi = ($hex | str substring 0..7 | into int --radix 16)
    let lo = ($hex | str substring 8..15 | into int --radix 16)
    (polars lit $hi | polars cast u64) * 4294967296 + $lo
}

# A UInt64 column as 16 lowercase hex digits.
export def hex-expr [c: string] {
    let digits = (0..15 | each {|i|
        let p = (15 - $i)
        let unit = (16 ** $p)
        (polars col $c) // $unit mod 16
            | polars replace --strict (0..15 | each {|d| $d }) (0..15 | each {|d| "0123456789abcdef" | str substring $d..$d }) --return-dtype str
    })
    polars concat-str "" $digits
}

def sql-name [n: string] { $"\"($n | str replace --all '"' '""')\"" }

# The mapping from one character of a rewritten text to its byte: "" (past the end) is 0.
def byte-map [] {
    let ascii = (0..127 | each {|i| char --integer $i })
    let high = (128..255 | each {|i| char --integer ($PRIVATE_BASE + $i) })
    {old: ([""] | append $ascii | append $high), new: ([0] | append (0..127) | append (128..255))}
}

# The non-ASCII characters present in the given text columns.
def wide-chars [lf, cols: list<string>] {
    $cols | each {|c|
        $lf
        | polars filter ((polars col $c | polars str-lengths --bytes) != (polars col $c | polars str-lengths))
        | polars select (polars col $c | polars str-split "" | polars as ch)
        | polars explode ch
        | polars filter ((polars col ch | polars str-lengths --bytes) > 1)
        | polars unique
        | polars collect
        | polars into-nu
        | get -o ch
        | default []
    } | flatten | uniq
}

# One XOR-and-multiply chain for a column's bytes, as SQL over columns __x<i> and __m<i>:
# h <- (h XOR x_i) * m_i for i in order.
def chain-sql [h: string, n: int] {
    0..<$n | reduce --fold $h {|i, acc| $"\(\(($acc) XOR __x($i)\) * __m($i)\)" }
}

# Adds the UInt64 columns ha and hb, the two hashes of each row, to a frame whose columns `cols`
# (text, null for NULL) are the row's values in order. Every other column is kept.
export def row-hashes [cols: list<string>] {
    let lf = ($in | polars into-lazy)
    let names = ($lf | polars schema | columns)
    for h in [ha hb] { if $h in $names { error make {msg: $"row-hashes: the frame already has a column ($h)"} } }
    let types = ($lf | polars schema)
    for c in $cols {
        if ($types | get $c) != "str" { error make {msg: $"row-hashes: column ($c) is ($types | get $c), not text"} }
    }
    let wide = (wide-chars $lf $cols)
    let bad = ($wide | where {|ch| $ch =~ "[\\x{E000}-\\x{E0FF}]" })
    if ($bad | is-not-empty) {
        error make {msg: "row-hashes: a text holds a character in U+E000..U+E0FF, which this digest uses internally"}
    }
    let rewrites = ($wide | each {|ch|
        let bytes = ($ch | into binary)
        let rep = (0..<($bytes | bytes length) | each {|i| char --integer ($PRIVATE_BASE + ($bytes | bytes at $i..$i | into int)) } | str join)
        {ch: $ch, rep: $rep}
    })
    let map = (byte-map)
    # rewritten texts __t<j>, their byte lengths __l<j>, and the longest per column
    let prepared = ($cols | enumerate | reduce --fold $lf {|it, acc|
        let t = ($rewrites | reduce --fold (polars col $it.item) {|r, e| $e | polars str-replace-all --pattern $r.ch --replace $r.rep })
        $acc | polars with-column ...[
            ($t | polars as $"__t($it.index)")
            (polars col $it.item | polars str-lengths --bytes | polars cast u64 | polars as $"__l($it.index)")
        ]
    })
    let longest = ($prepared | polars select ...($cols | enumerate | each {|it| polars col $"__l($it.index)" | polars max | polars as $"__l($it.index)" }) | polars collect | polars into-nu | first)
    let keep = ($names | append ($cols | enumerate | each {|it| [$"__t($it.index)" $"__l($it.index)"] } | flatten))
    let start = ($prepared | polars with-column ...[
        (u64-lit $OFFSET_A | polars as __ha)
        (u64-lit $OFFSET_B | polars as __hb)
    ])
    let hashed = ($cols | enumerate | reduce --fold $start {|it, acc|
        let j = $it.index
        let nn = (polars col $it.item | polars is-not-null)
        let len = (polars col $"__l($j)")
        let p = (polars lit $PRIME | polars cast u64)
        let one = (polars lit 1 | polars cast u64)
        let zero = (polars lit 0 | polars cast u64)
        let maxlen = ($longest | get $"__l($j)" | default 0)
        # the marker byte: 0 for NULL, 1 otherwise, always hashed
        let marker = [[(polars when $nn $one | polars otherwise $zero) $p]]
        # the length, four bytes little-endian, only when not NULL
        let length = (0..3 | each {|b|
            let unit = (256 ** $b)
            [((polars when $nn ($len // $unit mod 256) | polars otherwise $zero) | polars cast u64) (polars when $nn $p | polars otherwise $one)]
        })
        # the text bytes, position k while k is inside the text
        let text = (0..<$maxlen | each {|k|
            let active = ($nn and ($len > $k))
            let code = (polars col $"__t($j)" | polars str-slice $k --length 1 | polars replace --strict $map.old $map.new --return-dtype u64)
            [((polars when $active $code | polars otherwise $zero) | polars cast u64) (polars when $active $p | polars otherwise $one)]
        })
        let pass = ($keep | append [__ha __hb] | each {|c| sql-name $c } | str join ", ")
        # in chunks of 16 steps: the SQL parser refuses deeper nesting
        $marker | append $length | append $text | chunks 16 | reduce --fold $acc {|chunk, a|
            let with_steps = ($chunk | enumerate | reduce --fold $a {|s, b|
                $b | polars with-column ...[($s.item.0 | polars as $"__x($s.index)") ($s.item.1 | polars as $"__m($s.index)")]
            })
            let n = ($chunk | length)
            $with_steps | polars query $"SELECT ($keep | each {|c| sql-name $c } | str join ', '), (chain-sql '__ha' $n) AS __ha, (chain-sql '__hb' $n) AS __hb FROM df"
        }
    })
    let pass = ($keep | each {|c| sql-name $c } | str join ", ")
    let xor_shift = {|lf, s|
        let unit = (2 ** $s)
        $lf
        | polars with-column ...[((polars col __ha) // $unit | polars as __sa) ((polars col __hb) // $unit | polars as __sb)]
        | polars query $"SELECT ($pass), __ha XOR __sa AS __ha, __hb XOR __sb AS __hb FROM df"
    }
    let times = {|lf, k| $lf | polars with-column ...[((polars col __ha) * (u64-lit $k) | polars as __ha) ((polars col __hb) * (u64-lit $k) | polars as __hb)] }
    let mixed = (do $times (do $xor_shift (do $times (do $xor_shift $hashed 30) $MIX_1) 27) $MIX_2)
    do $xor_shift $mixed 31
    | polars rename [__ha __hb] [ha hb]
    | polars select ...($names | append [ha hb] | each {|c| polars col $c })
}

# The digest of a bag: per group, the row count (the sum of `mult`, Int64) and the two sums of
# mult x hash modulo 2^64 (UInt64 columns a and b). The frame needs the groups, `mult` (an integer)
# and the hashes ha and hb.
export def digest-sums [by: list<string>] {
    let lf = $in
    let m = (polars when ((polars col mult) >= 0) (polars col mult | polars cast u64)
        | polars otherwise ((polars lit 0 | polars cast u64) - (polars col mult | polars math abs | polars cast u64)))
    $lf | polars into-lazy
    | polars with-column ($m | polars as __m64)
    | polars group-by ...$by
    | polars agg [
        (polars col mult | polars cast i64 | polars sum | polars as rows)
        ((polars col __m64) * (polars col ha) | polars sum | polars as a)
        ((polars col __m64) * (polars col hb) | polars sum | polars as b)
    ]
}

# The digest's text: the two sums as 16 hex digits each.
export def digest-hex [] {
    $in | polars with-column (polars concat-str "" [(hex-expr a) (hex-expr b)] | polars as digest)
}


# Stops unless the digest of a fixed pair of rows is the one warren-bench records (its unit test
# `known_values` holds the same vector and the same text).
export def digest-self-check [] {
    let rows = ([
        [c0 c1 c2 c3 c4];
        ["1" "48379c01" "72" "f" null]
        ["2" "3001" "0" "t" ""]
    ] | polars into-df --schema {c0: str, c1: str, c2: str, c3: str, c4: str})
    let d = ($rows | polars into-lazy | polars with-column (polars lit 1 | polars as mult) (polars lit x | polars as g)
        | row-hashes [c0 c1 c2 c3 c4] | digest-sums [g] | digest-hex | polars collect | polars into-nu | first)
    let want = "2 a902184c0b59ad4e6b0e923d7126800a"
    if $"($d.rows) ($d.digest)" != $want {
        error make {msg: $"digest: the fixed test vector gives ($d.rows) ($d.digest), not ($want)"}
    }
}
