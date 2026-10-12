-- The purchases of one collection row, builder 2's row 6.
SELECT p.purchase_id, p.store, p.ordered_at, p.delivered_at
FROM lego.lego_purchases p
WHERE p.builder_id = 2 AND p.row_no = 6
