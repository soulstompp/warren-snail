-- Weather stations and their readings, as a snail holds them. The same file is PostgreSQL's DDL for
-- these tables.

CREATE FUNCTION weather.clock(timestamptz) RETURNS timestamp
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;

CREATE TABLE weather.stations (
    network_id integer NOT NULL,
    station_no integer NOT NULL,
    name character varying(80) NOT NULL,
    PRIMARY KEY (network_id, station_no)
);

CREATE INDEX s_name ON weather.stations (name, network_id, station_no);

CREATE TABLE weather.readings (
    reading_id bigint NOT NULL,
    network_id integer NOT NULL,
    station_no integer NOT NULL,
    feed character varying(32) NOT NULL,
    observed_at timestamptz NOT NULL,
    checked_at timestamptz,
    PRIMARY KEY (reading_id)
);

CREATE INDEX r_station ON weather.readings (network_id, station_no);
ALTER TABLE weather.readings CLUSTER ON r_station;

CREATE INDEX r_month ON weather.readings
    ((extract(month FROM weather.clock(observed_at))::smallint), observed_at) INCLUDE (checked_at);
