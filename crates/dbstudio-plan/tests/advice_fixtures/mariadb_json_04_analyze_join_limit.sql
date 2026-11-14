SELECT o.id, COUNT(i.id) FROM plan_fx_orders o JOIN plan_fx_items i ON i.order_id = o.id GROUP BY o.id ORDER BY COUNT(i.id) DESC LIMIT 10
