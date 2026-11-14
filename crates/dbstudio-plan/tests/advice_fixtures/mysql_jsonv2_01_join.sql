SELECT c.id, c.name, c.country, o.id, o.customer_id, o.status, o.amount, o.created FROM plan_fx_customers c JOIN plan_fx_orders o ON o.customer_id = c.id WHERE c.id < 50
