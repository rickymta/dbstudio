SELECT c.country, COUNT(*), SUM(o.amount) FROM customers c JOIN orders o ON c.id = o.customer_id WHERE o.status = 'open' GROUP BY c.country
