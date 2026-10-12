-- One set's purchases, Cafe Corner (10182-1), by month of the year.
SELECT extract(month FROM lego.clock(p.ordered_at))::smallint AS month, count(*) AS purchases
FROM lego.lego_collection c
JOIN lego.lego_purchases p ON p.builder_id = c.builder_id AND p.row_no = c.row_no
WHERE c.set_num = '10182-1'
GROUP BY 1
ORDER BY 1
