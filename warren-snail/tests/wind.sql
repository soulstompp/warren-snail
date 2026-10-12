-- The wind at each station, run by run. A run is the log's lines, in order, that come from one
-- station; each line is (seq, station, turn): seq its place in the log, station the station's code
-- as its feed sends it, turn the degrees the wind turned since the line before, clockwise positive.
-- A run's hours are its lines past whole days of $1 lines. Its turn is the net turn where the wind
-- never strayed more than $2 degrees from where the run began; where every turn was clockwise, the
-- net turn within the circle of $3 degrees; where every turn was counter-clockwise and the wind
-- never came round to where the run began, the same, counter-clockwise; and otherwise none, the
-- wind shifting, for the run's lines from first_seq to last_seq to be read on their own.
WITH bounds AS (
    SELECT CAST(CAST($1 AS text) AS bigint) AS per_day,
           CAST(CAST($2 AS text) AS numeric) AS steady,
           CAST(CAST($3 AS text) AS numeric) AS circle
), marked AS (
    SELECT seq, station, turn,
           CASE WHEN lag(station) OVER (ORDER BY seq) IS NOT DISTINCT FROM station THEN 0 ELSE 1 END AS starts
    FROM weather.wind
), numbered AS (
    SELECT seq, station, turn, sum(starts) OVER (ORDER BY seq) AS run
    FROM marked
), turned AS (
    SELECT run, seq, station, turn,
           sum(turn) OVER (PARTITION BY run ORDER BY seq ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS so_far
    FROM numbered
), runs AS (
    SELECT t.run, t.station, count(*) AS n, sum(t.turn) AS net, max(abs(t.so_far)) AS reach,
           bool_and(t.turn >= 0) AS veering, bool_and(t.turn <= 0) AS backing,
           bool_or(t.so_far < 0 AND mod(t.so_far, b.circle) = 0) AS came_round,
           min(t.seq) AS first_seq, max(t.seq) AS last_seq
    FROM turned t CROSS JOIN bounds b
    GROUP BY t.run, t.station
)
SELECT r.run, r.station,
       mod(r.n, b.per_day) AS hours,
       CASE WHEN r.reach <= b.steady THEN 'steady'
            WHEN r.veering THEN 'veered'
            WHEN r.backing AND NOT r.came_round THEN 'backed'
            ELSE 'shifting' END AS wind,
       CAST(CASE WHEN r.reach <= b.steady THEN r.net
            WHEN r.veering THEN mod(r.net, b.circle)
            WHEN r.backing AND NOT r.came_round THEN -mod(-r.net, b.circle)
            END AS text) AS turned,
       r.first_seq, r.last_seq
FROM runs r CROSS JOIN bounds b
ORDER BY r.run
