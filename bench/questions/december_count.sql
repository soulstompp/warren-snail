-- Every December's purchases, counted, on the UTC clock.
SELECT count(*) AS purchases
FROM lego.lego_purchases p
WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12
