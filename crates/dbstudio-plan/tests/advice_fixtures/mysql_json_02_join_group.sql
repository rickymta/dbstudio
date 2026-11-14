SELECT c.country, COUNT(*), SUM(o.amount) FROM plan_fx_orders o JOIN plan_fx_customers c ON c.id = o.customer_id WHERE o.created > DATE'2024-06-01' GROUP BY c.country ORDER BY COUNT(*) DESC
