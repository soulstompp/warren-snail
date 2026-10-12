-- The purchases of 2015 on the UTC clock, by the set each bought, read through the collection row
-- each purchase names.
SELECT c.set_num, count(*) AS purchases
FROM lego.lego_purchases p
JOIN lego.lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no
WHERE lego.clock(p.ordered_at) >= '2015-01-01' AND lego.clock(p.ordered_at) < '2016-01-01'
GROUP BY c.set_num
ORDER BY 1
