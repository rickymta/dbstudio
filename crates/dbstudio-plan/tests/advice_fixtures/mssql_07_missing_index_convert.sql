SELECT o.Total, o.Note FROM dbo.Orders o WHERE o.CustomerId = 42 AND o.Code = N'A-1234' AND o.OrderDate >= '2025-01-01'
