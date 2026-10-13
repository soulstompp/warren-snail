# warren-snail

warren-snail keeps a database in one file and reads it a column at a time. You declare its tables
with PostgreSQL's own DDL, and you ask it in PostgreSQL's SQL.

Each table lies in one order: the index it is clustered on, or its primary key. Each of its other
indexes is an order of its own, holding the columns the index names. Every order is cut into
blocks, and every block keeps counts of its rows on each place of its key. Before a round reads a
page of a column, it adds those counts up. It skips a block that holds none of the rows a question
asks for, answers a count from the counts alone, and of every other block reads only the pages
that hold the question's rows. It says which pages those are before it reads them, then reads
exactly those, and prints both.

The answer comes back as Arrow record batches, or as a Parquet file from the command. The snail's
pages are Parquet's own data pages, so a table goes out to a Parquet file, and comes back in, page
by page.

A statement that writes is a round too. A row lands in its block as it is written. A changed page
goes to a fresh page, and the commit is one write of the file's root, so a reader never waits for
a writer.

warren-snail runs on Linux.

## Install

```sh
cargo install --locked warren-snail
```

It needs Rust 1.88 or later. It installs one command, `warren-snail`, in `~/.cargo/bin`, and
nothing else; `cargo uninstall warren-snail` takes it out again. The library is the same crate:
`cargo add warren-snail`.

A snail is the file you name; past 1 GiB, Parquet's size for one file of huge data, it goes on
into `<file>.1`, `<file>.2` and on beside it. The snail is those files and nothing else: deleting
them deletes it.

## A first snail

A shop's orders, declared as PostgreSQL declares them, in `shop.sql`:

```sql
CREATE FUNCTION shop.clock(timestamptz) RETURNS timestamp
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;

CREATE TABLE shop.orders (
    order_id bigint NOT NULL,
    customer_id integer NOT NULL,
    placed_at timestamptz NOT NULL,
    total numeric(10,2) NOT NULL,
    PRIMARY KEY (order_id)
);

CREATE INDEX orders_customer ON shop.orders (customer_id, placed_at);
ALTER TABLE shop.orders CLUSTER ON orders_customer;

CREATE INDEX orders_month ON shop.orders
    ((extract(month FROM shop.clock(placed_at))::smallint), placed_at) INCLUDE (total);
```

The table is clustered on `orders_customer`, so each customer's orders lie together, in the order
they were placed. `orders_month` holds the orders by the month they were placed in. PostgreSQL
refuses `extract(month …)` on a `timestamptz` in an index, since the month depends on the
session's time zone; `shop.clock` reads the instant on UTC's clock, and the month through it is
indexable. The same file runs on PostgreSQL, in a database with a schema `shop`.

Write the snail from the DDL alone. Each order is written empty:

```sh
warren-snail write shop.snail --ddl shop.sql
```

```text
shop.orders orders_customer: 0 rows, 0 blocks, its directory 1 page in 1 level, its page maps 0 pages
    order_id       k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    customer_id    k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    placed_at      k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    total          k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
shop.orders orders_pkey: 0 rows, 0 blocks, its directory 1 page in 1 level, its page maps 0 pages
    order_id       k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    customer_id    k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    placed_at      k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
shop.orders orders_month: 0 rows, 0 blocks, its directory 1 page in 1 level, its page maps 0 pages
    placed_at      k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    total          k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    customer_id    k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
```

The table's order holds every column. The primary key's order holds `order_id`, then the table's
order's key; `orders_month` holds `placed_at`, the `total` it includes, and the rest of the table's
order's key. Each order ends in that key, which places a row in the table's order.

Each column's line counts its data pages, Parquet's own, and the disk pages they take. A disk page
is the file's page, 4 KiB, and every count of pages from here on is of disk pages.

Put five orders in, from `orders.sql`:

```sql
INSERT INTO shop.orders (order_id, customer_id, placed_at, total) VALUES
    (1, 7, '2026-01-05 10:00:00+00', 12.50),
    (2, 9, '2026-01-20 16:45:00+00', 30.25),
    (3, 7, '2026-02-01 09:30:00+00', 8.00),
    (4, 3, '2026-02-14 19:10:00+00', 54.90),
    (5, 9, '2026-03-02 11:05:00+00', 17.75);
```

```sh
warren-snail exec shop.snail orders.sql --out done.parquet
```

```text
1 statement, 18 pages written, generation 2
```

The statement wrote each of the three orders' new pages, and then the root, which names them. Each
commit is one generation of the file.

Ask for customer 7's orders, from `customer.sql`:

```sql
SELECT o.order_id, o.placed_at, o.total FROM shop.orders o WHERE o.customer_id = 7
```

```sh
warren-snail read shop.snail customer.sql --out answer.parquet
```

```text
customer.sql, round 1: 2 rows, along orders_customer
    pages: 3 as it began, 2 of the directory (0 held), 4 of columns predicted, 0 of them held and 4 read
    from the file: 9 pages, 36864 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_customer: 2 pages of its directory; 4 to 4 column pages by its directory, 4 predicted, 0 of them held
```

`answer.parquet` holds the two rows:

| order_id | placed_at | total |
|---|---|---|
| 1 | 2026-01-05 10:00:00+00 | 12.50 |
| 3 | 2026-02-01 09:30:00+00 | 8.00 |

The round read along `orders_customer`, the one order that holds every column the question reads.
As it began, it read the root's two copies and the catalog: 3 pages. It read 2 pages of the
order's directory, and from them it predicted that the question's rows lie in 4 column pages,
before it read any. It read those 4. That is 9 pages from the file, and the system's own count of
the bytes the program read agrees: 36,864 bytes.

## The table's order, and its indexes

The DDL says how each table lies. `ALTER TABLE … CLUSTER ON` names the index that is the table's
order; a table with none lies in its primary key. Each of its other B-tree indexes is an order of
its own, holding its key's columns and those it includes, then the table's order's key. A primary
key that is not the table's order is one too, and keeps its key unique.

**What it reads.** A question reads along an order that holds every column it reads. A question on
a few columns reads only those columns' pages, along whichever order holds them.

**The read it cancels.** The pages of every column the question does not read, and every page of
an order it does not read along.

**How it shows.** The first line of a round names the order it read along. `--order <name>` reads
along another, where that one holds the question's columns.

**Where it stops.** An index is held only as a B-tree, on columns or on the month of an instant on
UTC's clock. A question that reads a column no order holds in the question's own order still reads
along the table's order. Cluster each table on the index most of its questions lead with: a key
handed out as rows arrive, such as a sequence, lays the table out in the order the rows came,
which few questions ask for.

## Blocks, and the counts they keep

Every order is cut into blocks. A block is a run of its order's rows: a place of the order's key,
such as one customer, or one month of one year. A place splits into its parts where one of them
fills a page of every column the order holds; a part too thin to fill one joins the block beside
it, and a place none of whose parts fills a page stays one block. A block keeps counts, never one
number: the rows on each place of its key, the rows on each month of each instant it holds, each
column's NULLs, and each text column's bytes. Above the blocks, each order's directory keeps the
same counts summed, a level at a time, up to one page at the top.

**What it reads.** The directory's pages for the places the question fixes, from the top down.

**The read it cancels.** Every page of a block the counts say holds none of the question's rows,
and every column page of a count the counts answer. February's orders, counted:

```sql
SELECT count(*) AS orders FROM shop.orders o
WHERE extract(month FROM shop.clock(o.placed_at))::smallint = 2
```

```text
february_count.sql, round 1: 1 row, along orders_customer
    pages: 3 as it began, 3 of the directory (0 held), 0 of columns predicted, 0 of them held and 0 read
    from the file: 6 pages, 24576 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 0 blocks read
    orders_customer: 1 page of its directory; 0 to 0 column pages by its directory, 0 predicted, 0 of them held
    orders_pkey: 1 page of its directory; 0 to 0 column pages by its directory, 0 predicted, 0 of them held
    orders_month: 1 page of its directory; 1 to 1 column pages by its directory, left unread
```

The answer, 2, came from the directory's counts: no column page was read.

**How it shows.** The round's entries, each holding none of the question's rows, some, or all, and
the blocks it read.

**Where it stops.** The counts are kept down to the place that holds about a page. Below that,
each page names the first key it holds, so a block read in part reads the pages from the first
that may hold the question's rows to the last. A condition on a column outside the key reads that
column's pages to test it.

## Every order priced, the cheapest read

**What it reads.** Each order that holds the question's columns is priced from the top of its
directory, at the fewest and the most column pages it may read. An order whose fewest are more than
another's most is left unread. The rest walk down to their blocks' page maps, which predict their
pages exactly, and the order with the fewest pages from the file is read.

**The read it cancels.** Every order but one, most of them above their blocks.

**How it shows.** A line for each order the round priced: the pages of its directory it read, its
column pages by its directory, and its pages predicted, or `left unread`. February's orders, and
then the same question along `orders_month`:

```sql
SELECT o.placed_at, o.total FROM shop.orders o
WHERE extract(month FROM shop.clock(o.placed_at))::smallint = 2
```

```text
february.sql, round 1: 2 rows, along orders_customer
    pages: 3 as it began, 4 of the directory (0 held), 2 of columns predicted, 0 of them held and 2 read
    from the file: 9 pages, 36864 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_customer: 2 pages of its directory; 2 to 2 column pages by its directory, 2 predicted, 0 of them held
    orders_month: 2 pages of its directory; 2 to 2 column pages by its directory, 2 predicted, 0 of them held
```

```sh
warren-snail read shop.snail february.sql --out february.parquet --order orders_month
```

```text
february.sql, round 1: 2 rows, along orders_month
    pages: 3 as it began, 2 of the directory (0 held), 2 of columns predicted, 0 of them held and 2 read
    from the file: 7 pages, 28672 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_month: 2 pages of its directory; 2 to 2 column pages by its directory, 2 predicted, 0 of them held
```

On five rows both orders come to 2 column pages. Where two tie, the fewer pages touched decide,
then the fewer pages of the directory, then the order named first.

**Where it stops.** No statistics, no estimate: an order's price is its own pages, read from its
directory. A question's pages are predicted exactly once its order walks down to its page maps.

## Pages held between rounds

**What it reads.** An open snail holds the pages its rounds read, up to 128 MiB unless it is told
otherwise (PostgreSQL's default `shared_buffers`). Each round reads the root afresh from the file.
A held page is never stale: a page is written once, and a change goes to a fresh page.

**The read it cancels.** A held page is read from no file, and the pricing counts it as free, so a
round prefers the order whose pages it holds.

**How it shows.** The pages found held, beside those read from the file, and each order's pages
held. February's orders, asked twice of one open snail:

```sh
warren-snail read shop.snail february.sql --out february.parquet --rounds 2
```

```text
february.sql, round 1: 2 rows, along orders_customer
    pages: 3 as it began, 4 of the directory (0 held), 2 of columns predicted, 0 of them held and 2 read
    from the file: 9 pages, 36864 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_customer: 2 pages of its directory; 2 to 2 column pages by its directory, 2 predicted, 0 of them held
    orders_month: 2 pages of its directory; 2 to 2 column pages by its directory, 2 predicted, 0 of them held
february.sql, round 2: 2 rows, along orders_customer
    pages: 2 as it began, 4 of the directory (4 held), 2 of columns predicted, 2 of them held and 0 read
    from the file: 2 pages, 8192 bytes as the system counts them; 6 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_customer: 2 pages of its directory; 2 to 2 column pages by its directory, 2 predicted, 2 of them held
    orders_month: 2 pages of its directory; 2 to 2 column pages by its directory, 2 predicted, 0 of them held
```

The second round read the root's two copies from the file, and found the rest held.

**Where it stops.** When it is full, the page unused longest goes, never one the round in progress
has touched. `--hold <MiB>` sets the size. Built without the feature `standalone`, a snail holds no
pages of its own.

## Writing

`exec` runs each statement of a file in turn: `INSERT`, from values or from a question; `UPDATE`;
`DELETE`; `BEGIN`, `COMMIT` and `ROLLBACK`; and `CREATE TABLE`, with `CREATE INDEX` and
`ALTER TABLE … CLUSTER ON` for a table that holds no rows. Outside a transaction each statement
commits on its own.

**What it writes.** A row lands in its block on every order that holds its columns, as its places
name it, after the block's last rows, in pages of one disk page each, until a prune or a cut writes
the block again in order. An update marks the old row dead and chains it to its replacement; one that changes the
table's order's key moves the row to the block its new key names. Every changed page is written to
a fresh page, then the pages of the directory above it, then the root, in two copies written turn
about.

**The read it cancels.** A round reads the file as it stood when the round began, so a reader
never waits for a writer, and never reads a page half written. A write that stops before the root,
or halfway through it, leaves the file as it was.

**How it shows.** `--out` records each statement: its rows, its generation, and the pages it read,
wrote and reused. Changes to the shop, in `changes.sql`:

```sql
UPDATE shop.orders SET total = 9.50 WHERE order_id = 3;
UPDATE shop.orders SET customer_id = 3 WHERE order_id = 5;
DELETE FROM shop.orders WHERE order_id = 2;
BEGIN;
INSERT INTO shop.orders (order_id, customer_id, placed_at, total) VALUES (6, 7, '2026-03-09 08:20:00+00', 21.00);
COMMIT;
```

```text
6 statements, 57 pages written, generation 6
```

Each customer's orders afterwards:

```sql
SELECT o.customer_id, count(*) AS orders, sum(o.total) AS spent FROM shop.orders o
GROUP BY o.customer_id ORDER BY o.customer_id
```

| customer_id | orders | spent |
|---|---|---|
| 3 | 2 | 72.65 |
| 7 | 3 | 43.00 |

**Where it stops.** A writer locks the blocks it writes, never the file, so writers of different
blocks commit side by side; writers of one block take turns. Inside a transaction, a writer that
already holds blocks tries for another for ten seconds, then gives up, so two writers never wait on
each other forever. A read that pays a page or more of a block only for its dead rows, or for rows
written after its last, prunes it as the round ends, on a snail open to write: the block is written
again, in order and without its dead rows. A page the newest
root no longer names is freed once no older round still reads it, and reused:

```sh
warren-snail census shop.snail
```

```text
generation 6: 28 pages named, 0 free, 17 waiting to be freed
```

## Parquet, out and back in

**What it writes.** `out` writes each table to a Parquet file, one row group for each block of the
table's order, each block's pages copied as they lie, with the footer Parquet readers need. A block
that holds dead rows, or rows written after its sorted ones, is pruned on its way out.

```sh
warren-snail out shop.snail --dir parquet
```

```text
shop.orders: 5 rows in 1 row group in 1 file, 4 pages copied, 1 block pruned on the way: parquet/shop.orders.parquet
```

**The read it cancels.** A file's row groups already cut as the snail cuts them come back in by
their pages, encoded as they are. `write` takes each table's rows from a Parquet file, or from a
directory of them as `out` writes them:

```sh
warren-snail write copy.snail --ddl shop.sql shop.orders=parquet/shop.orders.parquet
```

```text
shop.orders orders_customer: 5 rows, 1 block, its directory 1 page in 1 level, its page maps 1 page
    order_id       k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 1 of 1 blocks
    customer_id    k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 1 of 1 blocks
    placed_at      k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 1 of 1 blocks
    total          k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 1 of 1 blocks
shop.orders orders_pkey: 5 rows, 1 block, its directory 1 page in 1 level, its page maps 1 page
    order_id       k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
    customer_id    k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
    placed_at      k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
shop.orders orders_month: 5 rows, 1 block, its directory 1 page in 1 level, its page maps 1 page
    placed_at      k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
    total          k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
    customer_id    k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
```

`moved 1 of 1 blocks`: the table's order came in by its pages. The other orders lie otherwise than
the file, and were encoded afresh.

**How it shows.** Each column of each order, the blocks that came by their pages and the pages it
takes. `k` is the most disk pages one data page of the column may take, sized from the largest
place of the order's key in any block, within the Parquet library's caps.

**Where it stops.** Pages stay uncompressed, and a column takes a dictionary only where that takes
fewer pages than its plain values. A row group that is not one of the snail's blocks, or a page
compressed or larger than its column's `k`, is encoded afresh.

## Files past 1 GiB

**What it writes.** A snail goes on into `<file>.1`, `<file>.2` and on every 1 GiB, as Parquet's
own guidance for huge data keeps a row group of that size to a file of its own. The root and the
locks stay in the first file. A run of pages never crosses from one file into the next: a run that
would starts the next file. A block written whole, by `write`, a prune or a cut, lies in one file.
A statement's changes take fresh pages wherever there is room, so a block changed since can lie in
two files until it is written whole again.

**The read it cancels.** A round opens a further file only when it reads a page there, so a file
that holds none of the question's pages is never opened.

**How it shows.** `blocks --out` records, for each column of each block, the files its page map
and its pages lie in.

**Where it stops.** A row group is never split between files. `out` writes a table larger than one
of the snail's files as a directory of numbered Parquet files, each holding whole row groups, and
one row group larger than a file in a file of its own; `write` reads such a directory back, its
files in turn. `write --file-pages <n>` makes a snail whose files hold `n` pages, to see the split
on small data.

## A question read as it goes: WITH and windows

**What it reads.** A question with a `WITH`, or a window, is read as a pipeline over one table,
along its table's order, folded as the blocks come.

**The read it cancels.** The table held whole, and a sort for the window. The table's order is the
window's order already, and each step holds only its running folds, its groups or the rows it
sorts.

**How it shows.** The round reads along the table's order, and `--round` records `rows_held`, the
most rows the pipeline held at once. A payments log, `payments.sql`:

```sql
CREATE TABLE shop.payments (
    seq bigint NOT NULL,
    customer_id integer NOT NULL,
    amount numeric(10,2) NOT NULL,
    PRIMARY KEY (seq)
);
```

Write it from its DDL, and put five payments in, from `payments-in.sql`:

```sql
INSERT INTO shop.payments (seq, customer_id, amount) VALUES
    (1, 7, 12.50), (2, 9, 30.25), (3, 7, 8.00), (4, 3, 54.90), (5, 9, 17.75);
```

```sh
warren-snail write payments.snail --ddl payments.sql
warren-snail exec payments.snail payments-in.sql --out paid.parquet
```

Each customer's paid so far, line by line, from `balance.sql`:

```sql
SELECT p.seq, p.customer_id,
       sum(p.amount) OVER (PARTITION BY p.customer_id ORDER BY p.seq) AS paid_so_far
FROM shop.payments p
```

```text
balance.sql, round 1: 5 rows, along payments_pkey
    pages: 3 as it began, 2 of the directory (0 held), 3 of columns predicted, 0 of them held and 3 read
    from the file: 8 pages, 32768 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 0 some, 1 all; 1 block read
    payments_pkey: 2 pages of its directory; 3 to 3 column pages by its directory, 3 predicted, 0 of them held
```

| seq | customer_id | paid_so_far |
|---|---|---|
| 1 | 7 | 12.50 |
| 2 | 9 | 30.25 |
| 3 | 7 | 20.50 |
| 4 | 3 | 54.90 |
| 5 | 9 | 48.00 |

**Where it stops.** A window is read only along the table's order, ascending, where that order is
one unique column. Its frame runs from the partition's first row to the current one. `--param`
gives `$1`, `$2` and on to a pipeline, as text.

## Checking an answer against PostgreSQL

The DDL is PostgreSQL's, and so is every question, so the same files run on
[PostgreSQL](https://www.postgresql.org/): load the DDL into a database with the schema it names,
put the same rows in, and ask the same question. PostgreSQL's rows are the answer the snail's are
checked against. Text is compared by its bytes, as PostgreSQL compares it under the `C` collation,
so check against a database whose collation is `C`, or `C.UTF-8`. A column that declares any
collation but `C` or `POSIX` is refused.

The composed statement [sql-composer](https://github.com/soulstompp/sql-composer) writes is plain
SQL in PostgreSQL's syntax, and the snail reads it as it reads any other question.

## Reference

### The commands

`warren-snail --help` prints each command and its flags.

- `write <file> --ddl <ddl.sql> [<table>=<rows.parquet>]... [--file-pages <n>]
  [--written <written.parquet>]`: a new snail, where no file is yet. A table given no rows is
  written empty. `--written` records each order's columns as Parquet.
- `exec <file> <statements.sql> --out <done.parquet>`: each statement in turn.
- `read <file> <question.sql> --out <answer.parquet> [--order <name>] [--param <value>]...
  [--rounds <n>] [--hold <MiB>] [--round <round.parquet>]`: the answer as Parquet, and each round.
- `out <file> --dir <directory>`: each table as Parquet.
- `blocks <file> --out <blocks.parquet>`: each column of each block, as the directory holds it.
- `census <file>`: every page of the file, named by the newest root, free, or waiting.
- `hold <file> --seconds <n>`: the snail held open, as a round that long holds it.

Each command exits 0, or prints why it refused and exits 1.

### The DDL it reads

- `CREATE TABLE`, its columns `NOT NULL` or not, and a `PRIMARY KEY`.
- Column types: `smallint`, `integer`, `bigint`; `numeric(p, s)` up to 18 digits, held as a
  64-bit integer of its smallest unit; `text` and `varchar(n)`, compared by their bytes; `bytea`;
  `timestamptz`, as microseconds on UTC's clock, its infinities among them.
- `CREATE INDEX`, a B-tree, its key on columns, on an instant's month,
  `(extract(month FROM <clock>(<column>))::smallint)`, and on the instant itself,
  `(<clock>(<column>))`, where `<clock>` is a function the DDL declares whose body is
  `$1 AT TIME ZONE 'UTC'`, as `shop.clock` above; `INCLUDE`; `UNIQUE`.
- `ALTER TABLE … CLUSTER ON <index>`. The index a table is clustered on may include columns; the
  table's order holds every column already.

### The SQL it reads

- **A question:** `SELECT` from one table, or two joined by an inner `JOIN … ON` equal columns;
  conditions joined by `AND`, each a column, or an instant's month on the clock, compared with a
  value by `=`, `<`, `<=`, `>` or `>=`; `count(*)`, and `sum` of an integer or numeric column;
  `GROUP BY`; `ORDER BY`. A join reads the table its own conditions narrow first, then keeps the
  other to the keys the first holds.
- **A pipeline,** a question with a `WITH` or a window: steps over one table and the steps before
  them, joined by `CROSS JOIN`; the windows `lag`, `sum`, `count` and `row_number`; the aggregates
  `count`, `sum`, `min`, `max`, `bool_and` and `bool_or`; `CASE`, `mod`, `abs`, casts, comparisons
  and `IS [NOT] DISTINCT FROM`; parameters `$1` and on.
- **A statement:** as `exec` above.

Anything else is refused, with a sentence that says what is not read.

### What the commands record

`read --round` writes one row a round. Its columns, in the order of the printed lines:

- `question`, `round_no`, `held_bytes` (what the snail may hold), `order` (the one read along),
  `served` (the orders that hold the question's columns), and `consulted` (each order priced, as
  `order:directory pages:fewest-most:predicted:held`);
- `rows`; `pages_at_open` (the root and the catalog, or the root alone after the first round);
  `directory_pages` and `directory_pages_held`;
- `column_pages_top_low` and `column_pages_top_high` (the order's price at its directory's top),
  `column_pages_low` and `column_pages_high` (at its blocks), `column_pages_predicted`,
  `column_pages_held` and `column_pages_read`;
- `pages_counted` (read from the file), `pages_held` (found held), `rchar_bytes` (the bytes the
  system counted, through `/proc/self/io`) and `rchar_whole_pages`;
- `entries`, `entries_none`, `entries_may` and `entries_must` (holding none of the question's rows,
  some, or all); `blocks_read`; `rows_held` (the most rows a pipeline held at once), `rows_walked`
  and `rows_taken` (rows the scan looked at one by one, and took in runs); `elapsed_us`, from
  opening the file on the first round, and from the round's start after.

`exec --out` writes one row a statement: `statement_no`, `statement`, `rows`, `generation`,
`pages_read`, `pages_written`, `blocks_rewritten`, `pages_reused`, `wchar_bytes` (the bytes the
system counted written) and `system_agrees` (whether they are the pages written). `exec` exits 1
where any statement's pages and the system's count disagree.

`blocks --out` writes one row for each column of each block: `table`, `order`, `block`, `depth`
and `place` (where the block lies in its order's key), `first` and `last` (its first and last
key), `rows`, `map_pages`, `column`, `k`, `dictionary_pages`, `data_pages`, `disk_pages`,
`pages_listed`, `bytes` (a text column's), `nulls`, `dead`, `dead_marked`, `chains` and `files`
(the files its page map and its pages lie in).

### The library

```rust,no_run
use std::path::Path;
use warren_snail::read::Snail;

fn main() -> warren_snail::Result<()> {
    let mut snail = Snail::open(Path::new("shop.snail"))?;
    let (answer, round) = snail.ask(
        "SELECT o.order_id, o.total FROM shop.orders o WHERE o.customer_id = 7",
        None,
    )?;
    println!("{} rows along {}", answer.num_rows(), round.order);

    let mut writer = Snail::open_writing(Path::new("shop.snail"))?;
    writer.execute("DELETE FROM shop.orders WHERE order_id = 6")?;
    Ok(())
}
```

- `Snail::open` opens a snail to read; `Snail::open_holding` sets what it holds, in bytes;
  `Snail::open_writing` opens it to write as well.
- `ask` answers a question along the cheapest order, or the one named, as an Arrow `RecordBatch`
  and the `Round` it took; `ask_with` takes the parameters of a pipeline.
- `execute` runs one statement; `out`, `blocks` and `census` are the commands of those names.
- `write::write_from` writes a new snail from a `catalog::Catalog` and each table's `write::Rows`.

### The feature `standalone`

On by default. With it, an open snail holds the pages its rounds read, and writes. Built without
it (`default-features = false`), a snail holds no pages of its own and refuses every write, for a
host that holds pages of its own.

## The bench

[`bench/`](https://github.com/soulstompp/warren-snail/tree/HEAD/bench) times the snail against
PostgreSQL on a database [sqlc-brickgen](https://crates.io/crates/sqlc-brickgen) builds, with the
DBA's indexes it puts on, every answer checked first.

## Licence

GPL-3.0-or-later. Copyright © Kenneth Allen Flegal.
