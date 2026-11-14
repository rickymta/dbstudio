select * from orders where customer_id not in (select id from banned) order by created limit 20 offset 50000
