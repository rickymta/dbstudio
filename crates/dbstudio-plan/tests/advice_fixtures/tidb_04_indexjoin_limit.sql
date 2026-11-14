SELECT * FROM customers c JOIN orders o ON o.customer_id = c.id WHERE c.id < 5 ORDER BY o.amount DESC LIMIT 10
