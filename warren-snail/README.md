# warren-snail

warren-snail keeps a database in one file and reads it a column at a time. You declare its tables
with PostgreSQL's own DDL, of PostgreSQL's own types, and you ask it in PostgreSQL's SQL.

Each table lies in one order: the index it is clustered on, or its primary key. Each of its other
indexes is an order of its own, holding the columns the index names. Every order is cut into
blocks, and every block keeps counts of its rows on each place of its key. Before a round reads a
page of a column, it adds those counts up. It skips a block that holds none of the rows a question
asks for, answers a count from the counts alone, and of every other block reads only the pages
that hold the question's rows. It says which pages those are before it reads them, then reads
exactly those, and prints both. A question may join any number of tables: each is read so, the one
its own conditions narrow first, and every other kept to the keys the tables before it hold.

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

Two features, off unless named, call a library of the machine's as PostgreSQL calls it:
`collations`, for text under ICU's collations and the C library's locales, linking the machine's
ICU, which pkg-config finds; and `zones`, for named time zones other than UTC, read from compiled
zone files. Built without them, such a column or such a zone is refused by name.

```sh
cargo install --locked warren-snail --features collations,zones
```

A snail is the file you name; past 1 GiB, Parquet's size for one file of huge data, it goes on
into `<file>.1`, `<file>.2` and on beside it. The snail is those files and nothing else: deleting
them deletes it. A snail made since 0.2.0 is of format 9, which 0.1.0 and 0.1.1 do not open; one
either made, of format 7 or 8, is opened and written in its own format.

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
    total.value    k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    total.nan      k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
shop.orders orders_pkey: 0 rows, 0 blocks, its directory 1 page in 1 level, its page maps 0 pages
    order_id       k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    customer_id    k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    placed_at      k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
shop.orders orders_month: 0 rows, 0 blocks, its directory 1 page in 1 level, its page maps 0 pages
    placed_at      k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    total.value    k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    total.nan      k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
    customer_id    k  1       0 data pages       0 disk pages  dictionary in 0 blocks  moved 0 of 0 blocks
```

The table's order holds every column. The primary key's order holds `order_id`, then the table's
order's key; `orders_month` holds `placed_at`, the `total` it includes, and the rest of the table's
order's key. Each order ends in that key, which places a row in the table's order.

`total` lies as two columns: `total.value`, its decimal, and `total.nan`, whether it is NaN, which
PostgreSQL's `numeric(p, s)` may hold. Each column's line counts its data pages, Parquet's own, and
the disk pages they take. A disk page is the file's page, 4 KiB, and every count of pages from here
on is of disk pages.

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
1 statement, 20 pages written, generation 2
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
    pages: 3 as it began, 2 of the directory (0 held), 5 of columns predicted, 0 of them held and 5 read
    from the file: 10 pages, 40960 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_customer: 2 pages of its directory; 5 to 5 column pages by its directory, 5 predicted, 0 of them held
```

`answer.parquet` holds the two rows, `total` as the same two parts:

| order_id | placed_at | total.value | total.nan |
|---|---|---|---|
| 1 | 2026-01-05 10:00:00+00 | 12.50 | false |
| 3 | 2026-02-01 09:30:00+00 | 8.00 | false |

The round read along `orders_customer`, the one order that holds every column the question reads.
As it began, it read the root's two copies and the catalog: 3 pages. It read 2 pages of the
order's directory, and from them it predicted that the question's rows lie in 5 column pages,
before it read any. It read those 5. That is 10 pages from the file, and the system's own count of
the bytes the program read agrees: 40,960 bytes.

## The table's order, and its indexes

The DDL says how each table lies. `ALTER TABLE … CLUSTER ON` names the index that is the table's
order; a table with none lies in its primary key. Each of its other indexes is an order of its own,
holding its key's columns and those it includes, then the table's order's key. A primary key that
is not the table's order is one too, and keeps its key unique.

A key column's NULLs lie on a run of their own after its values, as PostgreSQL's B-tree holds
them, or before them under `NULLS FIRST`, and the run is cut on the key's columns after it. So an
index on a nullable column loads as written, and `IS NULL` on its first column reads that run
alone.

**What it reads.** A question reads along an order that holds every column it reads. A question on
a few columns reads only those columns' pages, along whichever order holds them.

**The read it cancels.** The pages of every column the question does not read, and every page of
an order it does not read along.

**How it shows.** The first line of a round names the order it read along. `--order <name>` reads
along another, where that one holds the question's columns.

**Where it stops.** A B-tree is an order on its key: columns, an instant on UTC's clock, or that
instant's month. A hash index is an order on its key as a B-tree on it would be. A GiST or an
SP-GiST index on a range, a multirange, an `inet` or text is an order on the type's own order, each
block keeping the span of the values it holds. One on a point, a box, a polygon or a circle is an
order on x's and y's bits taken in turn, a shape by the centre of its bounding box, each block
keeping the bounding box of the shapes it holds, so that a box reads the blocks it meets and no
others, and `ORDER BY v <-> point LIMIT n` reads the nearest blocks first, stopping once its rows lie
nearer than the next block's box, a column the strand does not hold read after by the keys found,
where that reads fewer pages than the table's own read and its sort. A distance rounds as the
release the file records rounds it: 18 and 19 part in its last digit. A GIN index on an array, a
`jsonb` or a `tsvector`, and a GiST index on a `tsvector` or a `tsquery`, is a strand of what each row holds, an element and the row's key, on the element's order:
an operator finds its rows' keys there and the rows by them on the table's order, where that reads
the fewest pages, and one element's count comes from its counts. Every operator of those classes
reads so: a jsonpath's `@?` and `@@` by what must hold of a row it passes, a tsquery's `|`, `&`, `!`
and `<->`, an `OR` over them, `NOT` being held to each row. A GiST index with such a key beside a key
of another kind holds a strand of each. A BRIN index, on any of its operator
classes, stores no order of its own: each block of the table's order, and each directory entry above
the blocks, keeps a summary of the key, its least and greatest, its span, its runs of values, or its
bloom filter, and a block no value of which can hold the question's condition is never read. A
condition on a box column is read alike on any table: a block is ruled out by what it keeps of the
column, a strand's bounding box or the least and greatest of the box's corners every block keeps. A
partial index, its `WHERE` comparing its columns with values,
holds only the rows it names, and serves a question whose conditions imply its own. A question that
reads a column no order holds in the question's own order still reads along the table's order.
Cluster each table on the index most of its questions lead with: a key handed out as rows arrive,
such as a sequence or `uuidv7()`, lays the table out in the order the rows came, which few
questions ask for, and `gen_random_uuid()` scatters the rows across the blocks.

## Blocks, and the counts they keep

Every order is cut into blocks. A block is a run of its order's rows: a place of the order's key,
such as one customer, or one month of one year. A place splits into its parts where one of them
fills a page of every column that decides it. A column decides only through its values: a boolean
never does, nor does a column holding NULLs there whose values fill less than a page. A part too
thin to fill one joins the block beside it, and a place none of whose parts fills a page stays one
block. A block keeps counts, never one number: the rows on each place of its key, the rows on each
month of each instant it holds, each column's NULLs, and each text column's bytes. Above the
blocks, each order's directory keeps the same counts summed, a level at a time, up to one page at
the top. A count under `IS NULL` or `IS NOT NULL` is answered from the NULLs counted.

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
each page names the first key it holds, in its block's page map, which shares a page with the maps
of the blocks beside it, so a block read in part reads the pages from the first that may hold the
question's rows to the last. A condition on a column outside the key reads that column's pages to
test it.

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
    pages: 3 as it began, 4 of the directory (0 held), 3 of columns predicted, 0 of them held and 3 read
    from the file: 10 pages, 40960 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_customer: 2 pages of its directory; 3 to 3 column pages by its directory, 3 predicted, 0 of them held
    orders_month: 2 pages of its directory; 3 to 3 column pages by its directory, 3 predicted, 0 of them held
```

```sh
warren-snail read shop.snail february.sql --out february.parquet --order orders_month
```

```text
february.sql, round 1: 2 rows, along orders_month
    pages: 3 as it began, 2 of the directory (0 held), 3 of columns predicted, 0 of them held and 3 read
    from the file: 8 pages, 32768 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_month: 2 pages of its directory; 3 to 3 column pages by its directory, 3 predicted, 0 of them held
```

On five rows both orders come to 3 column pages. Where two tie, the fewer pages touched decide,
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
    pages: 3 as it began, 4 of the directory (0 held), 3 of columns predicted, 0 of them held and 3 read
    from the file: 10 pages, 40960 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_customer: 2 pages of its directory; 3 to 3 column pages by its directory, 3 predicted, 0 of them held
    orders_month: 2 pages of its directory; 3 to 3 column pages by its directory, 3 predicted, 0 of them held
february.sql, round 2: 2 rows, along orders_customer
    pages: 2 as it began, 4 of the directory (4 held), 3 of columns predicted, 3 of them held and 0 read
    from the file: 2 pages, 8192 bytes as the system counts them; 7 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 1 some, 0 all; 1 block read
    orders_customer: 2 pages of its directory; 3 to 3 column pages by its directory, 3 predicted, 3 of them held
    orders_month: 2 pages of its directory; 3 to 3 column pages by its directory, 3 predicted, 0 of them held
```

The second round read the root's two copies from the file, and found the rest held.

**Where it stops.** When it is full, the page unused longest goes, never one the round in progress
has touched. `--hold <MiB>` sets the size. Built without the feature `standalone`, a snail holds no
pages of its own.

## Writing

`exec` runs each statement of a file in turn, as written: `INSERT`, from values or from a question,
each column it leaves out or gives `DEFAULT` taking its `DEFAULT`; `UPDATE`, to values or to
`DEFAULT`; `DELETE`; `BEGIN`, `COMMIT` and `ROLLBACK`; `SET CONSTRAINTS`; and the DDL the snail
reads (*The DDL it reads*), a table's columns and its indexes changed only while it holds no rows,
and `ALTER TABLE … ADD CHECK` and `ADD FOREIGN KEY` each checked against the rows held unless `NOT
VALID`. Outside a transaction each statement commits on its own. A transaction runs at READ
COMMITTED unless told otherwise, as PostgreSQL has each level: READ UNCOMMITTED run as READ
COMMITTED, REPEATABLE READ, and SERIALIZABLE (below). `BEGIN` and `START TRANSACTION` may name one,
`SET TRANSACTION` names one for the transaction begun, another only before its first statement that
reads or writes, and `SET SESSION CHARACTERISTICS AS TRANSACTION` names one for the transactions
after; each takes `READ ONLY` or `READ WRITE`, a write in a transaction `READ ONLY` refused in
PostgreSQL's words, and `[NOT] DEFERRABLE`.

**What a row is given.** An identity's or a serial's value, or a `DEFAULT nextval(…)`, is its
sequence's next, drawn at the root's turn for the whole statement and never given back, as
PostgreSQL's `nextval` is not: a statement rolled back leaves its values unused. A writer holds a
sequence's `CACHE` values at a time. `now()`, `CURRENT_TIMESTAMP`, `transaction_timestamp()` and
the literals `'now'`, `'today'`, `'tomorrow'` and `'yesterday'` read one clock, the
transaction's start, wherever they are written; `statement_timestamp()` reads the statement's, and
`clock_timestamp()` the machine's. `gen_random_uuid()` and
`uuidv4()` hand out a version 4 uuid, `uuidv7()` a version 7, and the version 7 uuids one writer
hands out rise. A `STORED` generated column is computed at the write; a `VIRTUAL` one, PostgreSQL
18's default, at the read, and no order holds it. Each `CHECK` is checked at the write, passing
where it is NULL, and `NOT ENFORCED` checks nothing; each `UNIQUE` and `EXCLUDE` at the statement's
end. Each `FOREIGN KEY` is checked where PostgreSQL checks it: at the statement's end, or at the
commit where it is deferred, by its DDL or by `SET CONSTRAINTS`, on the rows the round wrote and
the keys it took from a parent. A reference holding a NULL references nothing under `MATCH SIMPLE`,
and under `MATCH FULL` holds only NULLs. Under `PERIOD`, a child's period lies within its parent's
periods together, and a parent's period taken leaves each child's period within it so held. A row
written locks the parent's block that holds its key, and a key taken the child's blocks that hold a
row referencing it. `CASCADE`, `SET NULL` and `SET DEFAULT` are written by the statement that takes
the key, deferred or not, as `RESTRICT` is checked by it; `NOT ENFORCED` checks and does nothing. A
commit whose deferred key finds an orphan rolls the transaction back. An `INSERT … SELECT` takes
each value by PostgreSQL's assignment casts: an integer or numeric rounded and held to its
column's range, any value as its text into a text column, a value into a column of its own type;
any other type refused, as PostgreSQL refuses it.

**What it writes.** A row lands in its block on every order that holds its columns, as its places
name it, after the block's last rows, in pages of one disk page each, until a prune or a cut writes
the block again in order. An update marks the old row dead and chains it to its replacement; one
that changes the table's order's key moves the row to the block its new key names. Every changed
page is written to a fresh page, then the pages of the directory above it, then the root, in two
copies written turn about.

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
6 statements, 56 pages written, generation 6
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
blocks commit side by side; writers of one block take turns, waiting as long as it takes. Each
statement reads what was committed when it began, and in a transaction its transaction's own
changes with it, as PostgreSQL's READ COMMITTED has it. A statement that waited for a block takes
the rows it found again in their newest versions, and only those still meeting its conditions.
Writers that wait in a ring, each for a block the next holds, fail one of their transactions with
`deadlock detected`; a transaction one of whose statements failed takes nothing but its end, which
rolls it back. A writer of 0.1.0 and one of a later version never write one snail at once: their
block locks lie on different bytes. A read that pays a page or more of a block only for its dead
rows, or for rows written after its last, prunes it as the round ends, on a snail open to write:
the block is written again, in order and without its dead rows. A page the newest root no longer
names is freed once no older round still reads it, and reused:

```sh
warren-snail census shop.snail
```

```text
generation 6: 30 pages named, 0 free, 17 waiting to be freed
```

### REPEATABLE READ

A transaction at `REPEATABLE READ` keeps its first statement's snapshot to its end, and holds that
generation, so the file grows while it runs. It reads each block as its snapshot holds it, with its
own rows, and its commit writes those same rows into the newest version of the block, so two
transactions writing different rows of one block both commit, neither waiting for the other. Only
the same row changed since the snapshot is refused, found by the table's key, SQLSTATE `40001`, in
PostgreSQL's words (`could not serialize access due to concurrent update`, or `delete`); a key another
holds is waited for, as PostgreSQL's unique check waits, and refused once that writer commits. What it
keeps of its rows until its commit counts in each round's room. A row changed and changed back since
the snapshot, or set to the value it held, is no change to the snail, where PostgreSQL refuses it.
A snail of format 7 or 8 holds a block in one version: a row written into a block another wrote since
the snapshot is refused by name, with the way across.

### SERIALIZABLE

A transaction at `SERIALIZABLE` reads one snapshot, as at `REPEATABLE READ`, and a monitor beside it
refuses the one that would keep the committed transactions from composing as one serial order, as
PostgreSQL's does, SQLSTATE `40001`, the failing statement and its words the same:

```text
could not serialize access due to read/write dependencies among transactions
DETAIL:  Reason code: Canceled on identification as a pivot, during commit attempt.
HINT:  The transaction might succeed if retried.
```

A retry is the application's, as on PostgreSQL. It blocks nothing but a `READ ONLY DEFERRABLE`
transaction's first read, which waits for a snapshot no transaction beside it can hurt. A transaction
marks each block its rounds look at, gaps and all, or an order whole past 32 of its blocks or where a
round read no block of it, as PostgreSQL's locks on pages and relations. Blocks are coarser than
PostgreSQL's tuples, so a transaction may be refused where a plan of PostgreSQL's that took its index
would commit, as one that scanned the table is refused: two `UPDATE`s by key of rows of one block
mark it, each its own scan, and the second to commit is refused; two `INSERT`s of one block read
nothing, and both commit. A marked read costs no page: the marks are locks on bytes past the file's pages. A committed transaction's lie in records
its root names, about a page a commit, while a round older than its commit runs; one that only read
writes them only where a transaction that can write at the level is open. The level is refused by
name on a snail of format 7 or 8, with the way across, and on a snail marked a replica, in PostgreSQL's
words. Across several snails opened together it holds as on one: the transaction takes its snapshot on
all of them at one moment, its marks and edges lie in each of their files, the test walks every file
the transactions it meets write, so a cycle through two snails is refused in the same words, and
commits take one order from a clock their roots share.

## Parquet, out and back in

**What it writes.** `out` writes each table to a Parquet file, one row group for each block of the
table's order, each block's pages copied as they lie, with the footer Parquet readers need. A block
that holds dead rows, or rows written after its sorted ones, is pruned on its way out.

```sh
warren-snail out shop.snail --dir parquet
```

```text
shop.orders: 5 rows in 1 row group in 1 file, 5 pages copied, 1 block pruned on the way: parquet/shop.orders.parquet
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
    total.value    k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 1 of 1 blocks
    total.nan      k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 1 of 1 blocks
shop.orders orders_pkey: 5 rows, 1 block, its directory 1 page in 1 level, its page maps 1 page
    order_id       k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
    customer_id    k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
    placed_at      k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
shop.orders orders_month: 5 rows, 1 block, its directory 1 page in 1 level, its page maps 1 page
    placed_at      k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
    total.value    k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
    total.nan      k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
    customer_id    k  1       1 data pages       1 disk pages  dictionary in 0 blocks  moved 0 of 1 blocks
```

`moved 1 of 1 blocks`: the table's order came in by its pages. The other orders lie otherwise than
the file, and were encoded afresh.

**How it shows.** Each column of each order, the blocks that came by their pages and the pages it
takes. `k` is the most disk pages one data page of the column may take, sized from the largest
place of the order's key in any block, within the Parquet library's caps.

**Where it stops.** Pages stay uncompressed, and a column takes a dictionary only where that takes
fewer pages than its plain values. A row group that is not one of the snail's blocks, a page
compressed or larger than its column's `k`, or a column on a dictionary where the counts would take
none, or the other way about, or whose dictionary ran out partway, is encoded afresh. A file `out`
wrote names the release of PostgreSQL its OIDs came from and the locale its money was written
under, and `write` takes them from it.

## Questions over several tables

**What it reads.** A question may join any number of tables: by `JOIN` on their columns compared,
by commas or `CROSS JOIN` met in `WHERE`, by `LEFT JOIN`, and by `RIGHT` or `FULL JOIN` as its first
join. Each table is read along an order of its own, as a question on it alone is: the table its own
conditions narrow first, then each other kept to the keys the tables read before it hold. The
copy's customers, from `customers.sql`:

```sql
CREATE TABLE shop.customers (
    customer_id integer NOT NULL,
    name text NOT NULL,
    PRIMARY KEY (customer_id)
);
INSERT INTO shop.customers (customer_id, name) VALUES (3, 'Ana'), (7, 'Bo'), (9, 'Cy');
```

```sh
warren-snail exec copy.snail customers.sql --out customers.parquet
```

```text
2 statements, 10 pages written, generation 3
```

Bo's orders, from `bo.sql`:

```sql
SELECT c.name, o.order_id, o.total FROM shop.customers c
JOIN shop.orders o ON o.customer_id = c.customer_id
WHERE c.name = 'Bo'
```

```sh
warren-snail read copy.snail bo.sql --out bo.parquet
```

```text
bo.sql, round 1: 3 rows, along customers_pkey+orders_customer
    pages: 3 as it began, 4 of the directory (0 held), 6 of columns predicted, 0 of them held and 6 read
    from the file: 13 pages, 53248 bytes as the system counts them; 0 pages found held
    the directory's entries: 2 looked at, 0 holding none of its rows, 2 some, 0 all; 2 blocks read
    customers_pkey: 2 pages of its directory; 2 to 2 column pages by its directory, 2 predicted, 0 of them held
    orders_customer: 2 pages of its directory; 4 to 4 column pages by its directory, 4 predicted, 0 of them held
```

| name | order_id | total.value | total.nan |
|---|---|---|---|
| Bo | 1 | 12.50 | false |
| Bo | 3 | 9.50 | false |
| Bo | 6 | 21.00 | false |

The round read the customers first, since its condition narrows them, then the orders along
`orders_customer`, kept to Bo's `customer_id`.

**The read it cancels.** Every block of a table the keys of the tables read before it rule out, as
a condition on the table would rule it out.

**How it shows.** The first line of the round names the orders it read along, joined by `+`, in the
order it read them; below, a line for each order it priced.

**Where it stops.** Two tables are met on their columns, by `=`, `<>`, `<`, `<=`, `>` or `>=`
where PostgreSQL compares their types so, a `FULL JOIN` by one equality at least. A join by `USING`
or `NATURAL`, a `RIGHT` or `FULL JOIN` after the first, and what else the question path does not
read over several tables is read as a pipeline (*A question read as it goes*), and so are a set
operation and a subquery. The rows each table gives the answer are held while the round meets them.
A table met more than once is read once for each, the second from the pages the first left held.

## Several snails, one round

**What it reads.** `--with <file>`, given as often as you like, opens one more snail beside the
first. A round reads every snail at one moment, and a question over tables of several snails reads
each table from its own, as one snail's tables are read.

**The read it cancels.** A copy of the tables into one file: each stays in its own.

**How it shows.** A line for each snail under the round: the pages of its root it read as the round
began, the order it read along, and its column pages predicted, held and read. `read --round`
records them as `snails`.

**Where it stops.** The snails hold no two tables of one name, as one database holds none.
`--order` is refused beside `--with`. In `exec`, each statement writes the snail that holds its
table, an `INSERT … SELECT` reading that snail alone, and no DDL is run. A transaction that writes
several snails commits them together, each of format 9, the first it wrote deciding: read together,
they stand all as they were before the commit, or all as it left them.

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

## Any number of tables

**What it writes.** The root names each order's directory, each money column's `lc_monetary`, the
catalog and where each sequence stands. While that fits the root's one page, the root holds it, as
it always has. Once it would not, in a file of format 9, it goes into lists of pages the root names:
the tables' orders and money columns, keyed by table, the top of their list in the root; the
catalog's text, a page a chunk; the sequences, by name. A commit writes again only the pages of a
list it changed, and those above them, each to a fresh page, then the root, which stays one small
write. A file of format 7 or 8 refuses a schema its root does not hold, by name.

**The read it cancels.** A round on one table of thousands reads the root, its table's directory
and its pages, never a page of another table's. An open snail keeps the pages of the lists it read,
and reads again only those a commit wrote since; opening a snail reads them all once.

**How it shows.** `Root::is_cut` says whether the root's lists lie in pages of their own; `census`
counts those pages among the pages the root names; `exec --out` records each statement's pages.

**Where it stops.** A row group is never split between files. `out` writes a table larger than one
of the snail's files as a directory of numbered Parquet files, each holding whole row groups, and
one row group larger than a file in a file of its own; `write` reads such a directory back, its
files in turn. `write --file-pages <n>` makes a snail whose files hold `n` pages, to see the split
on small data.

## Followers: the commit carried

**What it writes.** A snail open to write can be given a host, `Snail::carried_by`, that carries
its commits to other copies of the file. A commit is handed to the host once its pages are on the
disk, as a `carry::Commit`: every page it wrote, by its bytes, each named by its file and its page
there, and the root's bytes. The root is written only once the host returns, so a host can wait
until a majority of the copies holds the commit; meanwhile the commit keeps the root's turn, and
the next commit waits for it. `Commit::encode` gives the bytes to send, uncompressed, and
`Commit::decode` refuses bytes that are not a commit whole.

**The read it cancels.** A follower runs no statement. `carry::apply` writes a commit's pages where
they lie, then its root, so the follower stands at each of the leader's generations, its files byte
for byte the leader's. A new follower begins as `carry::copy`, every file of the leader's copied
whole while a round holds the generation the copy stands at.

**How it shows.** `census` on a follower names every page as on the leader, at the same generation.

**Where it stops.** A follower applies one commit at a time, only at the commit's parent; any other
is refused by name, and a commit already landed changes nothing. An apply stopped before the root,
or halfway through it, leaves the follower at the parent, and applied again it finishes. A round on
a follower reads its own generation, and an apply that frees pages an older round of the follower's
may read waits for that round to end. With no host, a commit writes its root as it always has.

## Inside a host that decides its commits

**What it writes.** `Snail::open_carried` opens a snail to write inside a host, PostgreSQL's
transaction for one, built with `standalone` or without. Each transaction's root carries the host's
tag for it, `Host::tag`, and is taken only where the host's judge, `carry::Judge`, says that
transaction committed; a sequence's advance lands at once. A host may keep the root, writing it
itself at its own word, and a judge may hand a round the root it takes, `Judge::root`
(`carry::taken_root`), in place of its word on each copy: a writer outside the host then chooses its
pages from the root the host checks its commit against. A host's transaction commits a snail once.

**The read it cancels.** A second commit deciding beside the host's: the host's own record of its
transaction decides alone.

**How it shows.** `Snail::open_judged` reads a snail as a judge decides its tagged roots; a round
with no judge reads the root before a tagged one, whole, and `census` reads that one. Where the root
taken holds a commit of a transaction the judge's round does not see, `Judge::sees`, the generation
that round sees is no longer in the file, and `Snail::unseen` names it. `carry::newest` is the
newest generation the file holds whole, taken or not.

**Where it stops.** A snail its host writes is written only through it: a writer with no judge is
refused by name. A host's recovery reads its log of commits again with `carry::replay`, which lands
a commit on its parent, over a root of the same generation the host never took, and leaves one the
file holds already or is past.

## A replica of a named source

**What it writes.** `Snail::mark_replica(Some(source))` marks a snail a replica of a source, by its
name: the mark lies in the root, written by a commit of its own, or by the transaction under way at
its COMMIT, and every commit after it carries it on. `mark_replica(None)` clears it, at cutover or
once the applier is gone for good. The command is `warren-snail replica`; a snail PostgreSQL writes
through its WAL is marked through PostgreSQL, so the mark reaches its standbys as any commit does.

**The read it cancels.** A second writer on a replica: while the mark stands, every writer is
refused by name, at its statement and again at its commit, where it reads the mark from the newest
root at the root's turn, but the one that presents itself as the source's applier,
`Snail::applying(source)` (`exec --applier`). A writer's read on a replica is pruned by the
applier's reads alone, and a block holding dead rows goes `out` only once they have been.

**How it shows.** `Snail::replica` names the source the root records; `warren-snail replica <file>`
prints it. A follower is its leader's generation byte for byte, so a leader's mark is its
followers', and `carry::apply` lands the leader's commits while a stray writer there is refused.

**Where it stops.** A snail marked a replica of one source is refused another's mark until it is
cleared; marking it as it is marked, or clearing a mark it does not hold, writes nothing. `recut`
carries the mark; `write` makes no snail where a replica lies. A snail of format 7 or 8 holds no
mark. The mark names a writer, and a source's name is presented by whoever writes it: PostgreSQL's
privileges, or the file's own, say who may write at all.

## A question read as it goes: the pipeline

**What it reads.** A question with a `WITH`, or a window, is read as a pipeline over one table,
along its table's order, folded as the blocks come; so is a question on one table that calls a
function, an operator or a pattern the question path does not read, an aggregate `DISTINCT`,
ordered or filtered, or a `LIMIT`, an `OFFSET` or a `FETCH FIRST`. Over several relations, each
step's `FROM` is met as one relation first: a join of tables in one round, as a question on them
is read, any other table alone by its own conditions; then steps, derived tables, `VALUES`, views
and a set-returning function's rows, under `LATERAL` or not, met by `ON`, `USING` or `NATURAL`,
`INNER`, `LEFT`, `RIGHT`, `FULL` or `CROSS`, and each step read over them. A set operation,
`UNION`, `INTERSECT` or `EXCEPT`, each with `ALL`, reads each arm as a step and meets their rows
by their values, NULLs as not distinct, each column typed as PostgreSQL's parser types it; in a
`WITH`, as a derived table, or sorted and limited over the whole. A subquery (scalar, `EXISTS`,
`IN`, `ANY` or `ALL`, and `LATERAL` in `FROM`) that reads nothing of the query around it is made
once; one that reads it is made for each row's values of what it names, its tables read once and
its rows met with each row by the `=`s that tie them, as PostgreSQL answers it, `NOT IN` beside a
NULL and `ALL` over no rows among it. `WITH RECURSIVE` makes a query that reads itself round by
round, as PostgreSQL does: its non-recursive term once, then its recursive term over the rows the
round before added, until a round adds none, `UNION` keeping only rows not made before; each
column typed by the non-recursive term; its queries made in the order they read each other. A
recursion that makes more than 1,048,576 rows, a row group's, is refused by name, as are its
`SEARCH` and `CYCLE` clauses.

**The read it cancels.** The table held whole, and a sort for the window. The table's order is the
window's order already, and each step holds only its running folds, its groups or the rows it
sorts. A window on any other order holds its step's rows and sorts each partition on its own
order.

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
    pages: 3 as it began, 2 of the directory (0 held), 4 of columns predicted, 0 of them held and 4 read
    from the file: 9 pages, 36864 bytes as the system counts them; 0 pages found held
    the directory's entries: 1 looked at, 0 holding none of its rows, 0 some, 1 all; 1 block read
    payments_pkey: 2 pages of its directory; 4 to 4 column pages by its directory, 4 predicted, 0 of them held
```

| seq | customer_id | paid_so_far |
|---|---|---|
| 1 | 7 | 12.50 |
| 2 | 9 | 30.25 |
| 3 | 7 | 20.50 |
| 4 | 3 | 54.90 |
| 5 | 9 | 48.00 |

**Where it stops.** A window folds as the blocks come only along a table's order, where a step
reads that table alone, ascending, where that order is one unique column, and its frame runs from
the partition's first row to the current one; any other window, on another order, `DESC` or
`NULLS FIRST`, over a join, a frame offset from the current row, `GROUPS` or `EXCLUDE`, reads its
step's rows whole. A window over a grouped step is refused by name. `--param` gives `$1`, `$2` and
on to a pipeline, as text. `warren-snail functions` counts the functions PostgreSQL defines: those
the snail answers, those it does not read, and the server's own, which it refuses by name; `--out`
lists each.

## Checking an answer against PostgreSQL

The DDL is PostgreSQL's, and so is every question, so the same files run on
[PostgreSQL](https://www.postgresql.org/): load the DDL into a database with the schema it names,
put the same rows in, and ask the same question. PostgreSQL's rows are the answer the snail's are
checked against. Text that names no collation is compared by its bytes, as PostgreSQL compares it
under the `C` collation, so check against a database whose collation is `C`, or `C.UTF-8`. A
column's own collation orders it as PostgreSQL orders it: one that orders by bytes in any build,
and with the feature `collations` one of ICU's or the C library's. Each expression's collation is
derived as PostgreSQL derives it: a column's own, a literal's the default, a `COLLATE` over
either, and two that conflict refused.

[warren-snail-pg](https://github.com/soulstompp/warren-surveyor/tree/HEAD/warren-snail-pg) mounts
a snail in PostgreSQL as a schema of foreign tables, so the snail's answers and PostgreSQL's are
asked in one database. Each scan of one of its tables is a round of the snail's own, and `EXPLAIN`
prints it.

The composed statement [sql-composer](https://github.com/soulstompp/sql-composer) writes is plain
SQL in PostgreSQL's syntax, and the snail reads it as it reads any other question.

## Reference

### The commands

`warren-snail --help` prints each command and its flags, and `warren-snail <command> --help` that
command's alone.

- `write <file> --ddl <ddl.sql> [<table>=<rows.parquet>]... [--file-pages <n>]
  [--release <14..19>] [--lc-monetary <locale>] [--text-search-config <name>]
  [--written <written.parquet>]`: a new snail, where no file is yet. A table given no rows is
  written empty. The release of PostgreSQL the rows' OIDs came from, the locale their money was
  written under and the `default_text_search_config` every round answers under are those the files
  name; where they name none, `--release`, `--lc-monetary` and `--text-search-config` give them, 19,
  `C` and `pg_catalog.simple` unless given, a configuration the snail does not read refused by
  name. `--written` records each order's columns as Parquet.
- `exec <file> <statements.sql> --out <done.parquet> [--with <file>]... [--host <command>
  --host-server <name>] [--applier <source>]`: each statement in turn. `--host` writes a snail
  PostgreSQL writes through its WAL, as one transaction of the server's (warren-snail-pg's *Writing
  through PostgreSQL*); `--applier` writes as a source's applier, the one writer of its replica.
- `replica <file> [--of <source> | --clear]`: the source the snail is marked a replica of; `--of`
  marks it, `--clear` clears it.
- `read <file> <question.sql> --out <answer.parquet> [--order <name>] [--param <value>]...
  [--rounds <n>] [--hold <MiB>] [--room <bytes>] [--handed] [--round <round.parquet>] [--text]
  [--with <file>]...`: the answer as Parquet, and each round. `--room` gives each round that many
  bytes for its rows and the pages it keeps, the rows past them written once and read back once.
  `--handed` reads it as a host hands it, what it holds of its answer counted in that room. `--text`
  writes each value of a time type, a scalar's and an interval's, as PostgreSQL's output writes it.
- `out <file> --dir <directory>`: each table as Parquet.
- `blocks <file> --out <blocks.parquet>`: each column of each block, as the directory holds it.
- `census <file>`: every page of the file, named by the newest root, free, or waiting.
- `recut <file> --to <new file>`: the snail written afresh, every order cut under the versions of
  the collations this build reads.
- `catalog (<file> | --ddl <ddl.sql> [--taken <taken.sql>] [--statements <statements.parquet>])
  [--out <catalog.parquet>]`: what the snail reads of a DDL, a statement at a time, each taken or
  refused by name, and the catalog's facts as Parquet.
- `functions [--release <14..19>] [--out <functions.parquet>]`: every function PostgreSQL defines,
  and whether the snail answers it.
- `types [--release <14..19>] [--out <types.parquet>]`: every type PostgreSQL defines, and whether
  the snail takes a table's column of it, or why not.
- `operators [--release <14..19>] [--out <operators.parquet>] [--setup <setup.sql>]`: every
  operator PostgreSQL defines, and whether a pipeline's step, a plain question and a WHERE each
  answer it, or why not.
- `opclasses [--release <14..19>] [--out <opclasses.parquet>]`: every operator class PostgreSQL
  defines, and whether the snail takes an index of it, or why not.
- `hold <file> --seconds <n>`: the snail held open, as a round that long holds it.

Each command exits 0, or prints why it refused and exits 1.

### The DDL it reads

- `CREATE TABLE`, its columns `NOT NULL` or not, each with a `DEFAULT`, `GENERATED ALWAYS AS (…)
  STORED` or `VIRTUAL`, `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY (…)`, or of a serial type;
  a `PRIMARY KEY`; `UNIQUE`, `NULLS [NOT] DISTINCT`, `DEFERRABLE` and `INCLUDE`; `CHECK`, `NOT
  ENFORCED`; `EXCLUDE`; `REFERENCES` and `FOREIGN KEY`, a primary key, a `UNIQUE` or a unique index
  of a table of the file referenced, `MATCH SIMPLE` or `FULL`, `ON DELETE` and `ON UPDATE` each `NO
  ACTION`, `RESTRICT`, `CASCADE`, `SET NULL` or `SET DEFAULT`, with the columns it sets,
  `DEFERRABLE`, `INITIALLY DEFERRED`, `NOT ENFORCED`, and in `ALTER TABLE … ADD`, `NOT VALID`;
  `PERIOD` before the last column on both sides, under `NO ACTION`, referencing a key `WITHOUT
  OVERLAPS` or an `EXCLUDE` whose last column it is. A `PRIMARY KEY` or `UNIQUE` whose last column,
  a range or a multirange, is `WITHOUT OVERLAPS` is an `EXCLUDE USING gist`, `=` on its other
  columns and `&&` on that one, an empty period refused; a primary key so written with no `CLUSTER
  ON` is the table's order. Unnamed keys, checks, foreign keys and sequences are named as
  PostgreSQL names them. `write` checks each foreign key over the rows it is given, and says where
  its columns lead no order, so that a key taken from its parent reads the child whole. A table
  needs a primary key or a `CLUSTER ON`; a temporary or an unlogged table, a partitioned one and
  one that inherits are refused.
- `CREATE SEQUENCE` and `ALTER SEQUENCE`, their options as PostgreSQL reads them, a sequence's
  state kept in the file; `CREATE TYPE … AS ENUM`, a composite, `AS RANGE`, with `ALTER TYPE … ADD
  VALUE` and `RENAME VALUE`; `CREATE DOMAIN` and `ALTER DOMAIN`; `CREATE COLLATION`, of byte order,
  and with the feature `collations` of ICU or of the C library; `CREATE FUNCTION … LANGUAGE sql`
  whose body is one expression; `CREATE VIEW`, with its column list, its question read over the
  tables and views before it as the view is made, and read as a step of each question that names
  it.
- `CREATE INDEX`: `USING btree`, `hash`, `gist`, `spgist`, `gin` or `brin`, as *The table's order, and its
  indexes* says; a key on columns, on a column under its own `COLLATE`, on an instant on UTC's
  clock, `(<column> AT TIME ZONE 'UTC')`, on its month, `(extract(month FROM
  <clock>(<column>))::smallint)`, or on a function the DDL declares, read through its body, as
  `shop.clock` above; `INCLUDE`; `UNIQUE`, which takes any number of NULLs unless it says `NULLS
  NOT DISTINCT`; a `WHERE` comparing its columns with values, or testing them for NULL or truth;
  each key's operator class, its type's default or, on text, one comparing its bytes; `WITH (…)`,
  read and doing nothing; a key column's `NULLS FIRST` or `NULLS LAST`; and `DESC`, its values held
  rising all the same, giving the side its NULLs take, which `catalog` names.
- `ALTER TABLE … CLUSTER ON <index>`, a B-tree or a GiST index that is not partial, and not on a
  `tsvector` or a `tsquery`, whose strand holds a row once per element. The index a
  table is clustered on may include columns; the table's order holds every column already.

### The types it holds

Each type is read and written in PostgreSQL's own spellings, compared as PostgreSQL compares it,
and assigned from another type as PostgreSQL's casts take it.

- `smallint`, `integer`, `bigint` and the serial types; `real`, `double precision` and `float(p)`,
  each value kept as its own bits, `-0` equal to `0` and every NaN equal and above `Infinity`;
  `numeric(p, s)` up to 38 digits as a decimal beside whether it is NaN, and `numeric` of no limit,
  or past 38 digits, as its text; `money`, its cents, under the locale its file records.
- `text`, `varchar(n)`, `character(n)`, `name` and `"char"`; `bytea`; `bit(n)` and
  `bit varying(n)`; `boolean`; `uuid`.
- `date`, `time`, `timetz`, `timestamp`, `timestamptz`, each with its precision, and `interval`,
  with its fields; an instant on UTC's clock, its infinities among them.
- `oid`, `oid8`, `xid`, `xid8`, `cid`, `pg_lsn` and `tid`; the reg* types, `regclass`, `regtype`,
  `regproc`, `regprocedure`, `regoper`, `regoperator`, `regnamespace`, `regrole`, `regcollation`,
  `regconfig`, `regdictionary` and `regdatabase`, each written and read as the name of a built-in
  object of the release the file's OIDs came from, which the snail carries for 14 to 19, and any
  other OID bare.
- `macaddr`, `macaddr8`, `inet` and `cidr`; `json`, `jsonb` and `jsonpath`; `xml`; `tsvector` and
  `tsquery`; `refcursor`, `aclitem`, `pg_snapshot` and `txid_snapshot`; `point`, `line`, `lseg`,
  `box`, `path`, `polygon` and `circle`; `pg_node_tree` and the planner's statistics types.
- An array of any of them; `int2vector` and `oidvector`, arrays counted from 0 in their own text;
  an enum, a composite, a range and its multirange, as the DDL declares them, and the built-in
  ranges and multiranges; a domain; a table's row type; and each composite in `pg_catalog`, with
  the fields of the release the file's OIDs came from, but those holding a pseudo-type, which
  PostgreSQL refuses too.

`xid`, `cid`, the geometric types, `json` and `xml` key no B-tree. Any other type is refused by
name.

### The SQL it reads

- **A question:** `SELECT` from one table or several (*Questions over several tables*); conditions
  joined by `AND`, each a column, or an instant's month on the clock, compared with a value by `=`,
  `<>`, `<`, `<=`, `>` or `>=`, `BETWEEN`, a boolean column alone, under `NOT`, or under `IS [NOT]
  TRUE`, `IS [NOT] FALSE` or `IS [NOT] UNKNOWN`, a column under `IS NULL` or `IS NOT NULL`, or a
  text column against a pattern written out by `LIKE`, `ILIKE`, `SIMILAR TO` or `~`, its fixed
  prefix narrowing the read; `count(*)`; `sum`, `min`, `max` and `avg`; `GROUP BY`; `ORDER BY`.
- **A pipeline,** a question with a `WITH` or a window, or one the question path does not read (*A
  question read as it goes*): steps over tables, the steps before them, derived tables, `VALUES`,
  views and set-returning functions in `FROM`, met by `ON`, `USING` or `NATURAL`; `*` and `t.*`;
  `DISTINCT`; `HAVING`; `LIMIT`, `OFFSET` and `FETCH FIRST`, `WITH TIES` among them; `GROUP BY` and
  `ORDER BY` naming an item of the select list by its place or its name, `ORDER BY` an expression
  it does not return; the windows `row_number`, `rank`, `dense_rank`, `percent_rank`, `cume_dist`,
  `ntile`, `lag`, `lead`, `first_value`, `last_value`, `nth_value` and the aggregates, on any order,
  partitioned or not, named in a `WINDOW` clause, their frames `ROWS`, `RANGE` and `GROUPS` offset
  from the current row, whole partitions and `EXCLUDE`, and in a file of release 19 `RESPECT` and
  `IGNORE NULLS`; `CASE`, `COALESCE`, `LEAST`, `GREATEST`, `NULLIF`, `IN`, casts, comparisons and
  `IS [NOT] DISTINCT FROM`; `ORDER BY … USING` an operator a B-tree class orders by, or `~<~` and
  `~>~`, wherever an `ORDER BY` stands; `TABLESAMPLE SYSTEM` and `BERNOULLI`, `REPEATABLE` or not,
  a table's rows drawn by its counts and only the pages of what is drawn read;
  the functions `warren-snail functions` names held and the operators `warren-snail operators`
  names held, regular expressions among them, matching as PostgreSQL's own engine matches;
  parameters `$1` and on. Each step's expressions are typed before
  any row is read, so a function or an aggregate PostgreSQL has no form of for its arguments is
  refused over no rows too.
- **A statement:** as `exec` above.

Anything else is refused, with a sentence that says what is not read: among it a recursive query's
`SEARCH` and `CYCLE` clauses, a window over groups, a locking clause (`FOR UPDATE`, `FOR NO KEY
UPDATE`, `FOR SHARE`, `FOR KEY SHARE`), the record image's order (`*<`, `*<=`, `*>`, `*>=` and `record_image_ops`), which is PostgreSQL's in-memory layout of each type, and in a write `RETURNING`, `ON CONFLICT`, `UPDATE …
FROM`, `DELETE … USING`, `TRUNCATE`, `MERGE` and a savepoint. A statement sqlparser reads otherwise than it is written is refused too, so
nothing it declares is dropped without a word: what PostgreSQL's grammar has no place for (`TOP`,
`LATERAL VIEW`, `CONNECT BY`, `QUALIFY`, `FOR XML`, a string as a label, a table's hints) in its
words, and `SELECT … INTO`, which makes a table with no key, a session's or an unlogged one, by why.

### What the commands record

`read --round` writes one row a round. Its columns, in the order of the printed lines:

- `question`, `round_no`, `held_bytes` (what the snail may hold), `order` (the one read along),
  `served` (the orders that hold the question's columns), and `consulted` (each order priced, as
  `order:directory pages:fewest-most:predicted:held`);
- `rows`; `pages_at_open` (the root and the catalog, or the root alone after the first round);
  `directory_pages`, `directory_pages_predicted` and `directory_pages_held`;
- `column_pages_top_low` and `column_pages_top_high` (the order's price at its directory's top),
  `column_pages_low` and `column_pages_high` (at its blocks), `column_pages_predicted`,
  `column_pages_held` and `column_pages_read`;
- `pages_counted` (read from the file), `pages_held` (found held), `rchar_bytes` (the bytes the
  system counted, through `/proc/self/io`) and `rchar_whole_pages`;
- `entries`, `entries_none`, `entries_may` and `entries_must` (holding none of the question's rows,
  some, or all); `blocks_read`; `rows_held` (the most rows a pipeline held at once), `rows_walked`
  and `rows_taken` (rows the scan looked at one by one, and took in runs); `elapsed_us`, from
  opening the file on the first round, and from the round's start after; `snails` (each snail's
  part of a round across several, as `root pages:predicted:held:read`).

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
- `execute` runs one statement; `out`, `blocks`, `census` and `recut` are the commands of those
  names.
- `read::Snails` opens several snails together, to read (`open`, `open_holding`) or to write
  (`open_writing`), and asks and executes as one snail does.
- `hand` gives a snail, or `Snails`, a function of the caller's, called from SQL by its name as any
  function is: `read::handed::Function` makes a scalar, an aggregate (its `handed::Fold` taking a
  group's rows in the order its `ORDER BY` gives) or a set-returning one, of the argument types it
  names; `handed::row_type` names the row of several columns one returns.
- `write::write_from` writes a new snail from a `catalog::Catalog` and each table's `write::Rows`.
- `carried_by` gives a snail open to write its `carry::Host`; `carry::apply` and `carry::copy` keep
  a follower.
- `open_carried` opens a snail to write inside a host that tags and decides its roots, and
  `open_judged` reads one, `unseen` naming a commit its judge's round does not see;
  `carry::taken_root` is the root a judge hands a writer outside the host, `carry::newest` the
  newest generation a file holds; `carry::replay` and `carry::replay_held` land a host's log of
  commits.
- `mark_replica` marks a snail a replica of a source, or clears the mark; `replica` names it;
  `applying` makes a writer that source's applier; `Commit::replica` is the mark a commit's root
  carries.

### The features

- `standalone`, on by default. With it, an open snail holds the pages its rounds read, and writes.
  Built without it (`default-features = false`), a snail holds no pages of its own and writes only
  inside a host, for a host that holds pages of its own.
- `collations`, off by default: text under ICU's collations and the C library's locales, ordered as
  PostgreSQL orders it, the machine's ICU linked as PostgreSQL links it; nondeterministic
  collations with ICU's. Each order of such text is cut under the version of its collation, and an
  order cut under another version is refused until `recut` writes the snail afresh. Without the
  feature, a DDL naming such a collation is refused, a snail written with one is read but where its
  order is needed, and money under a locale other than `C` or `POSIX` is refused.
- `zones`, off by default: named time zones other than UTC, read from compiled zone files, the
  machine's `/usr/share/zoneinfo` unless `SNAIL_ZONES` names another directory. Without it, a value
  in such a zone is refused by name.

## The bench

[`bench/`](https://github.com/soulstompp/warren-snail/tree/HEAD/bench) times the snail against
PostgreSQL on a database [sqlc-brickgen](https://crates.io/crates/sqlc-brickgen) builds, with the
DBA's indexes it puts on, every answer checked first.

## Licence

GPL-3.0-or-later. Copyright © Kenneth Allen Flegal. Portions are under the notices in
[NOTICE](NOTICE), each given whole: what the crate translates from PostgreSQL, the regular
expression package PostgreSQL holds, and Snowball.
