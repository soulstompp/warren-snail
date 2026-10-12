-- When each purchase ordered in a December was delivered.
SELECT p.delivered_at
FROM lego.lego_purchases p
WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12
