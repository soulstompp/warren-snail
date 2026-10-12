# warren-snail

warren-snail keeps a database in one file and reads it a column at a time. Its tables are declared
with PostgreSQL's own DDL, and it is asked in PostgreSQL's SQL. Each block of a table keeps counts
of its rows, so a round skips the blocks that hold none of a question's rows, answers a count from
the counts alone, and reads only the pages it predicted it would read.

- [`warren-snail/`](warren-snail/README.md): the crate, a library and the command `warren-snail`.
  Its page runs a first snail, then each read it makes, then the reference.
- [`bench/`](bench/README.md): the bench, which times the snail against PostgreSQL on a database
  [sqlc-brickgen](https://crates.io/crates/sqlc-brickgen) builds, with the DBA's indexes it puts
  on, every answer checked first.

warren-snail runs on Linux.

## Licence

GPL-3.0-or-later. Copyright © Kenneth Allen Flegal.
