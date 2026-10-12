-- Every purchase, by the store it came from.
SELECT p.store, count(*) AS purchases
FROM lego.lego_purchases p
GROUP BY p.store
ORDER BY 1
