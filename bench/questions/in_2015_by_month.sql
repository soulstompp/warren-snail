-- The purchases of 2015 on the UTC clock, by month.
SELECT extract(month FROM lego.clock(p.ordered_at))::smallint AS month, count(*) AS purchases
FROM lego.lego_purchases p
WHERE lego.clock(p.ordered_at) >= '2015-01-01' AND lego.clock(p.ordered_at) < '2016-01-01'
GROUP BY 1
ORDER BY 1
