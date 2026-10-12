# The bench

The bench asks nine questions of PostgreSQL and of the snail, on the whole of a database
[sqlc-brickgen](https://crates.io/crates/sqlc-brickgen) builds, with the DBA's indexes it puts on.
Before any time is taken, it checks each of the snail's answers against PostgreSQL's, by its count
of rows and a digest of them. Then it times each side, and reads the pages each side touched first,
the time second.

It runs on Linux, as the snail does, and it is written in Nushell with its polars plugin.

## What it needs

- A PostgreSQL server you can create a database on, and `psql`.
- sqlc-brickgen 0.0.2 or later, whose DBA's indexes `lego.sql` declares:
  `cargo install --locked sqlc-brickgen`.
- Nushell, with its polars plugin registered by `plugin add`. Run the scripts with `nu`: under
  `nu --no-config-file`, Nushell finds no plugin.
- The snail's command built in this workspace: `cargo build --release`.

## 1. Build the database

```sh
createdb lego
curl -sSfL https://raw.githubusercontent.com/neondatabase/postgres-sample-dbs/main/lego.sql \
    | psql -X -q -v ON_ERROR_STOP=1 -o /dev/null -d lego
sqlc-brickgen --database-url postgres:///lego --size small --indexes keys,composite
```

The first two make the database and load the real catalogue into its `public` schema.
sqlc-brickgen reads it and writes the schema `lego`: the catalogue, and a large made-up one built
from it, with builders, their collections and their purchases. `--indexes keys,composite` builds
the DBA's indexes the bench reads: the primary keys, and the composite indexes for the joins
between the tables. It creates no extension. sqlc-brickgen's default builds those and the search
indexes, whose extensions only a superuser may create.

## 2. Write the snail

```sh
nu bench/load.nu postgres:///lego
```

`load.nu` reads two of sqlc-brickgen's tables, the purchases and the collection, and writes them
whole into a snail declared by [`lego.sql`](lego.sql): the two tables with every column, their
keys, and sqlc-brickgen's composite indexes on them, each as sqlc-brickgen writes it. One line is
the DBA's own: the purchases are clustered on their index by collection row, which makes it their
order in the snail. PostgreSQL's side is left as sqlc-brickgen built it.

It stops first where the database lacks one of the indexes `lego.sql` names. It ends by checking
that the snail holds each table as PostgreSQL does: its rows, each column's NULLs, and each text
column's bytes. It writes `bench/out/<database>/`: the two tables as Parquet, the snail
`<database>.snail`, and `blocks.parquet`, each block of each order. It exits 1 where it stops or
the check fails. `--seed` leaves one row of the collection out, which the check must refuse.

## 3. Run the bench

```sh
nu bench/bench.nu postgres:///lego
```

For each question, in turn:

- **the certificate:** the snail's answer, against the question's file run twice on PostgreSQL;
  RIGHT where its rows and their digest agree, WRONG otherwise;
- **a probe:** the snail once, which caps PostgreSQL at ten times its time, a second at least;
- **the repetitions,** five a side unless `--runs` says otherwise, the first side alternating by
  question. Each PostgreSQL repetition is a fresh session running
  `EXPLAIN (ANALYZE, BUFFERS, TIMING OFF, FORMAT JSON)`. Each snail repetition is a fresh process
  asking the question twice of one open snail: its first round, timed from opening the file, and
  its second, which finds held the pages the first read.

It writes `bench/out/<database>/<UTC time>/`: `certificate.parquet`, `runs.parquet`,
`machine.parquet` (the load and the other sessions, before and after), `settings.parquet`
(PostgreSQL's version, `shared_buffers`, `session_preload_libraries`, the indexes on the two
tables, and their sizes) and `summary.md`. The summary gives each question's pages first:
PostgreSQL's pages, 8 KiB each as PostgreSQL is built by default, hit in shared buffers and read
from outside them, and the snail's 4 KiB pages read from its file or found held. Then the time, as
median ± MAD [min..max] over the repetitions, RIGHT answers only. A question is a clear win or a
clear loss only where the two sides' times do not overlap at all.

It exits 1 on any WRONG answer. `--seed` leaves one row out of every snail answer before its
digest, which must make every answer WRONG.

## The questions

Each is a file in [`questions/`](questions), run as written on both sides.

- `builder_sets`: one builder's collection rows and the set each holds.
- `by_month`: every purchase, by the month of the year on UTC's clock.
- `by_store`: every purchase, by the store it came from.
- `december_count`: every December's purchases, counted.
- `december_delivered`: when each purchase ordered in a December was delivered.
- `in_2015_by_month`: the purchases of 2015, by month.
- `in_2015_by_set`: the purchases of 2015, by the set each bought, through its collection row.
- `one_row`: the purchases of one collection row.
- `one_set_by_month`: one set's purchases, by month of the year.

## What it reads, and what it changes

Every session the bench opens is read only (`default_transaction_read_only`) and names itself
`warren-snail-bench` in `pg_stat_activity`. It writes no row, changes no setting, and starts or
stops nothing. It refuses to start while another client session is on the server, since one would
move PostgreSQL's times. What it measures is the database as it stands: an index or a setting
added there is measured with it, and `settings.parquet` records them.

The digest is
[warren-bench](https://github.com/soulstompp/warren-surveyor/tree/HEAD/warren-bench)'s, written
again in Nushell in [`digest.nu`](digest.nu): each answer's rows counted and digested as
warren-bench digests its own. It checks itself against warren-bench's recorded test vector each
run.

## Where the data comes from, and its terms

The bench ships no data: step 1 downloads the real catalogue, and sqlc-brickgen builds the rest
from it and from the cities it ships, Natural Earth's populated places (public domain). The
catalogue is Neon's sample dump, `lego.sql` in
[neondatabase/postgres-sample-dbs](https://github.com/neondatabase/postgres-sample-dbs), a
repository under the MIT licence, whose README gives the data's source as Kaggle's
[LEGO Database](https://www.kaggle.com/datasets/rtatman/lego-database) (rtatman), which Kaggle lists
under CC0. The data comes originally from [Rebrickable](https://rebrickable.com), and the bench
follows Rebrickable's terms: they allow it to be used for any purpose, commercial use included, ask
that it be credited as sourced from Rebrickable (credit Rebrickable wherever you use it), and
forbid using any Rebrickable content to train AI models.

LEGO® is a trademark of the LEGO Group, which does not sponsor, authorise or endorse this project.

## Licence

GPL-3.0-or-later. Copyright © Kenneth Allen Flegal.
