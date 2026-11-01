# Changelog

## 0.2.0

PostgreSQL's types, its other index methods, questions over any number of tables, several snails
at once, and a snail written inside a host that decides its commits.

### What a user of 0.1.1 meets

- **Format 9.** A snail written by 0.2.0 is of format 9, which 0.1.0 and 0.1.1 do not open. A snail
  of format 7 or 8 is opened and written in its own format, and what that format cannot hold is
  refused by name.
- **A `numeric(p, s)` lies as two columns,** its decimal and whether it is NaN, which PostgreSQL's
  `numeric` may hold: `write` and `blocks` name them `total.value` and `total.nan` for a column
  `total`. An answer's `numeric(p, s)` column is an Arrow struct of the two,
  `{value: Decimal128(p, s), nan: Boolean}`, where 0.1.1 gave the `Decimal128` alone; a sum, an
  average and a pipeline's numeric column are as they were.
- **The API is closed** so that later types and fields come in minor releases: every public enum,
  and 21 of 0.1.1's public structs, are `#[non_exhaustive]`. A `match` on one of the enums needs a
  wildcard arm, and those structs are not built by a literal outside the crate, nor taken apart
  without `..`.
- Two types of the question reader that no public path reaches changed too: `Interval`'s bounds
  are 128 bits, where they were 64, and `Side`'s conditions are a `Cond` each.
- **A `bigint`'s sum is a numeric,** a `Decimal128(38, 0)` named `numeric`, as PostgreSQL's
  `sum(bigint)` is, where 0.1.1 gave a `bigint` that wrapped past its range; a `smallint`'s and an
  `integer`'s are `bigint`s, in a pipeline too.
- **A locking clause is refused by name,** `FOR UPDATE` and the others, where 0.1.1 answered the
  question and took no lock.

### Added

- The types: the integers and serials, the floats, `numeric` past 18 digits, `money`; `text`,
  `varchar`, `character`, `name`, `"char"`, `bytea`, the bit strings, `boolean`, `uuid`; the date
  and time types and `interval`; `oid`, `oid8`, `xid`, `xid8`, `cid`, `pg_lsn`, `tid` and the reg*
  types, named by the release of 14 to 19 the file's OIDs came from; the network types, JSON,
  `jsonpath`, XML, text search, the geometric types, the snapshots, `aclitem` and the planner's
  statistics types; arrays, enums, composites, ranges and multiranges, domains and a table's row
  type.
- The DDL, read a statement at a time, each statement taken or refused by name, and one sqlparser
  reads otherwise than it is written refused: `DEFAULT`, identities, serials and sequences, a
  sequence's state kept in the file; `STORED` and `VIRTUAL` generated columns; `CHECK`, `NOT
  VALID`, `VALIDATE CONSTRAINT`; `UNIQUE` and `NULLS NOT DISTINCT`; `EXCLUDE` and `WITHOUT
  OVERLAPS`; foreign keys, `MATCH SIMPLE` and `FULL`, every action, deferred or not, and `PERIOD`;
  `CREATE TYPE`, `CREATE DOMAIN`, `CREATE COLLATION`, `CREATE FUNCTION … LANGUAGE sql`; operator
  classes, `WITH`, `NULLS FIRST` and `DESC`.
- The indexes: hash, and GiST and SP-GiST on a range, a multirange, an `inet` or text, each an
  order the counts read; a partial index, read where a question implies its `WHERE`.
- The questions: any number of tables, by `JOIN`, commas, `CROSS JOIN`, `LEFT`, `RIGHT` and `FULL
  JOIN`, met by `=`, `<>`, `<`, `<=`, `>` or `>=`; the conditions `<>`, `BETWEEN`, `IS [NOT] NULL`,
  answered from the NULLs counted, and the truth tests; `LIKE`, `ILIKE`, `SIMILAR TO` and `~`, a
  pattern's fixed prefix narrowing the read; `min`, `max` and `avg` on the question path.
- The pipeline: `DISTINCT`; `LIMIT`, `OFFSET` and `FETCH FIRST`, `WITH TIES` among them, a question
  on one table taking them, or a `CASE`, a document's arrows or a parameter, read as a pipeline; a
  parameter typed by its context, as a quoted literal is; `GROUP BY` and `ORDER BY` naming a select
  list's item by its place or its name; more windows and frames; the functions and operators of
  PostgreSQL's types, regular expressions matching as PostgreSQL's own engine matches; the command
  `functions` lists every function PostgreSQL defines and whether the snail answers it.
- The commands `types`, `operators` and `opclasses` list every type, operator and operator class
  PostgreSQL 14 to 19 defines, each held or refused in the snail's own words: a type by a table's
  column of it, an operator by a pipeline's step over columns of its types, an operator class by
  an index of it.
- Writes: an `INSERT … SELECT` held to PostgreSQL's assignment casts, `SET CONSTRAINTS`, and the
  DDL above in `exec`; the `DEFAULT`s `now()`, `gen_random_uuid()`, `uuidv4()` and `uuidv7()`;
  READ COMMITTED, and READ UNCOMMITTED run as it, named by `BEGIN`, `START TRANSACTION`, `SET
  TRANSACTION` and `SET SESSION CHARACTERISTICS AS TRANSACTION`.
- One clock a transaction: `now()`, `CURRENT_TIMESTAMP`, `transaction_timestamp()` and the literals
  `'now'`, `'today'`, `'tomorrow'` and `'yesterday'` read the transaction's start wherever they are
  written, `statement_timestamp()` the statement's, `clock_timestamp()` the machine's.
- Several snails at once: `--with` on `read` and `exec`, and `read::Snails`: a round reads them at
  one moment, a question joins their tables, and a transaction that writes several commits them
  together, one root deciding; `read --round` records each snail's part as `snails`.
- Followers: `Snail::carried_by`, `carry::Commit`, `carry::apply` and `carry::copy` carry each
  commit to other copies of the file.
- A host that decides its commits: `Snail::open_carried`, `Snail::open_judged`, `carry::Judge`,
  `carry::replay`; and `exec --host`, a snail written through PostgreSQL's WAL.
- A replica of a named source: `Snail::mark_replica`, `Snail::replica`, `Snail::applying` and
  `Commit::replica`; the command `replica` and `exec --applier`. While the mark stands, every writer
  is refused by name but the source's applier.
- The features `collations`, ICU's collations and the C library's locales, and `zones`, named time
  zones read from compiled zone files; the command `recut`, an order cut under its collation's
  version written afresh. Each expression's collation derived as PostgreSQL derives it, two that
  conflict refused.
- The commands `catalog`, what the snail reads of a DDL, a statement at a time; `read --text`;
  `write --release` and `--lc-monetary`, for a Parquet file that names neither, and refused where
  they disagree with one that does; each command's own `--help`.

## 0.1.1

- Format 8: the page maps of blocks side by side share pages. A snail of 0.1.0's format 7 is read
  and written as it lies.
- A row written past a block's last rows goes into the block's tail, and a block written whole lies
  in one file.
- Writers run as PostgreSQL's READ COMMITTED: a writer waits for a block as long as it takes, and a
  ring of writers fails one of its transactions as a deadlock.
- A Parquet column comes in by its pages only where its encoding is the one the counts would take.
- The library: `Snail::price` prices a question from its directory alone, `Snail::scan` and
  `Snail::next_block` hand its round out a block at a time, `Snail::ask_each` hands each block's
  rows to a function as they are read, and `Catalog::function_statements` gives the functions the
  DDL declares.

## 0.1.0

The first release: a database in one file, read a column at a time, declared in PostgreSQL's DDL
and asked in its SQL, on Linux.

- Each table in one order, the index it is clustered on or its primary key, and each of its other
  B-tree indexes an order of its own; every order cut into blocks, each block keeping counts of its
  rows on each place of its key, and each order's directory those counts summed.
- A question priced on every order that holds its columns, the cheapest read, its pages predicted
  exactly before any is read; a count answered from the counts alone.
- `smallint`, `integer`, `bigint`, `numeric(p, s)` up to 18 digits, `text`, `varchar(n)`, `bytea`
  and `timestamptz`, and indexes on an instant on UTC's clock and its month.
- Questions on one table or two joined on equal columns, and a pipeline for `WITH` and windows.
- Writes: `INSERT`, `UPDATE`, `DELETE` and transactions, writers locking blocks, never the file, a
  reader never waiting; dead rows pruned as a round reads them.
- Parquet out and back in by its pages, and files past 1 GiB.
- The commands `write`, `exec`, `read`, `out`, `blocks`, `census` and `hold`, and the library.
