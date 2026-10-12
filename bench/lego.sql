-- The purchases and the collection of a database sqlc-brickgen builds, as a snail holds them:
-- the two tables with every column, their primary keys, and the composite indexes sqlc-brickgen
-- builds on them, each as sqlc-brickgen writes it. One line is the DBA's own: the purchases are
-- clustered on their index by collection row, which makes it the order the snail lays them out in.

CREATE FUNCTION lego.clock(timestamptz) RETURNS timestamp
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;

CREATE TABLE lego.lego_collection (
    builder_id integer NOT NULL,
    row_no integer NOT NULL,
    set_num character varying(255) NOT NULL,
    typed_set_num character varying(255),
    typed_name character varying(255),
    PRIMARY KEY (builder_id, row_no)
);

CREATE INDEX lego_collection_set_num_builder_id_row_no_idx ON lego.lego_collection
    (set_num, builder_id, row_no);
CREATE INDEX lego_collection_builder_id_row_no_idx ON lego.lego_collection
    (builder_id, row_no) INCLUDE (set_num);

CREATE TABLE lego.lego_purchases (
    purchase_id bigint NOT NULL,
    builder_id integer NOT NULL,
    row_no integer NOT NULL,
    store character varying(64) NOT NULL,
    ordered_at timestamptz NOT NULL,
    ordered_local character varying(32) NOT NULL,
    delivered_at timestamptz,
    PRIMARY KEY (purchase_id)
);

CREATE INDEX lego_purchases_builder_id_row_no_month_ordered_at_idx ON lego.lego_purchases
    (builder_id, row_no, (extract(month FROM lego.clock(ordered_at))::smallint), (lego.clock(ordered_at)))
    INCLUDE (ordered_at, purchase_id);
CREATE INDEX lego_purchases_month_ordered_at_idx ON lego.lego_purchases
    ((extract(month FROM lego.clock(ordered_at))::smallint), (lego.clock(ordered_at)))
    INCLUDE (builder_id, row_no, ordered_at, purchase_id);

ALTER TABLE lego.lego_purchases CLUSTER ON lego_purchases_builder_id_row_no_month_ordered_at_idx;
