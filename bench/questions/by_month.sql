-- Every purchase, by month of the year on the UTC clock.
SELECT extract(month FROM lego.clock(p.ordered_at))::smallint AS month, count(*) AS purchases
FROM lego.lego_purchases p
GROUP BY 1
ORDER BY 1
