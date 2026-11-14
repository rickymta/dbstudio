SELECT c.country, COUNT(*), SUM(o.amount) FROM plan_fx_customers c JOIN plan_fx_orders o ON o.customer_id = c.id WHERE o.created > '2024-06-01' GROUP BY c.country ORDER BY COUNT(*) DESC
