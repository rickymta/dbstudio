CREATE SCHEMA sales;
GO
CREATE TYPE dbo.Email FROM NVARCHAR(100) NOT NULL;
GO
CREATE TYPE sales.LineList AS TABLE (sku NVARCHAR(20) NOT NULL, qty INT NOT NULL DEFAULT 1);
GO
CREATE SEQUENCE sales.order_seq AS BIGINT START WITH 1000 INCREMENT BY 5;
GO
CREATE TABLE sales.customers (
  id INT IDENTITY(1,1) NOT NULL CONSTRAINT PK_customers PRIMARY KEY,
  name NVARCHAR(100) NOT NULL,
  email dbo.Email NULL,
  credit DECIMAL(12,2) NOT NULL CONSTRAINT DF_customers_credit DEFAULT (0),
  created DATETIME2(3) NOT NULL CONSTRAINT DF_customers_created DEFAULT (SYSUTCDATETIME()),
  notes NVARCHAR(MAX) NULL,
  CONSTRAINT UQ_customers_name UNIQUE (name),
  CONSTRAINT CK_customers_credit CHECK (credit >= 0)
);
GO
CREATE TABLE sales.orders (
  id INT NOT NULL,
  line INT NOT NULL,
  customer_id INT NOT NULL,
  qty INT NOT NULL,
  price DECIMAL(10,2) NOT NULL,
  total AS (qty * price) PERSISTED,
  ver ROWVERSION,
  CONSTRAINT PK_orders PRIMARY KEY (id, line),
  CONSTRAINT FK_orders_customer FOREIGN KEY (customer_id) REFERENCES sales.customers(id) ON DELETE CASCADE
);
GO
CREATE INDEX IX_orders_customer ON sales.orders (customer_id DESC) INCLUDE (qty) WHERE qty > 0;
GO
CREATE TABLE dbo.nokey (a INT, b NVARCHAR(10));
GO
CREATE TABLE dbo.uniq_only (code NVARCHAR(10) NOT NULL, v INT NULL);
GO
CREATE UNIQUE INDEX UX_code ON dbo.uniq_only (code);
GO
CREATE VIEW sales.big_orders AS SELECT id, line, total FROM sales.orders WHERE total > 100;
GO
CREATE FUNCTION sales.fn_double(@x INT) RETURNS INT AS BEGIN RETURN @x * 2; END;
GO
CREATE FUNCTION sales.fn_orders(@c INT) RETURNS TABLE AS RETURN (SELECT id, line FROM sales.orders WHERE customer_id = @c);
GO
CREATE PROCEDURE sales.sp_customer_count AS BEGIN SELECT COUNT(*) AS n FROM sales.customers; END;
GO
CREATE TRIGGER sales.trg_customers_ins ON sales.customers AFTER INSERT AS BEGIN SET NOCOUNT ON; END;
GO
CREATE SYNONYM dbo.cust FOR sales.customers;
GO
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Customer master', @level0type = N'SCHEMA', @level0name = N'sales', @level1type = N'TABLE', @level1name = N'customers';
EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Display name', @level0type = N'SCHEMA', @level0name = N'sales', @level1type = N'TABLE', @level1name = N'customers', @level2type = N'COLUMN', @level2name = N'name';
GO
INSERT INTO sales.customers (name, email, credit) VALUES (N'Ada Lovelace', N'ada@example.com', 100.50), (N'Grace Hopper', NULL, 0), (N'Linus 100% _x_', N'l@x.org', 7), (N'Zoë 😀', NULL, 1);
INSERT INTO sales.orders (id, line, customer_id, qty, price) VALUES (1, 1, 1, 2, 60.00), (1, 2, 1, 1, 5.50), (2, 1, 2, 3, 10.00);
INSERT INTO dbo.nokey VALUES (1, N'a'), (2, N'b');
INSERT INTO dbo.uniq_only VALUES (N'x', 1), (N'y', 2);
GO
CREATE TABLE dbo.paging (id INT NOT NULL PRIMARY KEY, grp INT NOT NULL, label NVARCHAR(20) NOT NULL);
GO
INSERT INTO dbo.paging (id, grp, label)
SELECT n, n % 7, N'row-' + CAST(n AS NVARCHAR(10))
FROM (SELECT TOP (2500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n FROM sys.all_objects a CROSS JOIN sys.all_objects b) x;
GO
CREATE TABLE dbo.types_all (
  id INT NOT NULL PRIMARY KEY,
  c_bit BIT, c_tinyint TINYINT, c_smallint SMALLINT, c_int INT, c_bigint BIGINT,
  c_decimal DECIMAL(38,10), c_numeric NUMERIC(10,0), c_money MONEY, c_smallmoney SMALLMONEY,
  c_real REAL, c_float FLOAT, c_char CHAR(5), c_varchar VARCHAR(20), c_nchar NCHAR(5), c_nvarchar NVARCHAR(100),
  c_text TEXT, c_ntext NTEXT, c_xml XML, c_binary BINARY(4), c_varbinary VARBINARY(MAX), c_image IMAGE,
  c_rowversion ROWVERSION, c_guid UNIQUEIDENTIFIER, c_date DATE, c_time TIME(7), c_smalldatetime SMALLDATETIME,
  c_datetime DATETIME, c_datetime2 DATETIME2(7), c_dto DATETIMEOFFSET(7), c_variant SQL_VARIANT,
  c_geography GEOGRAPHY, c_hierarchyid HIERARCHYID
);
GO
INSERT INTO dbo.types_all (id, c_bit, c_tinyint, c_smallint, c_int, c_bigint, c_decimal, c_numeric, c_money, c_smallmoney,
  c_real, c_float, c_char, c_varchar, c_nchar, c_nvarchar, c_text, c_ntext, c_xml, c_binary, c_varbinary, c_image,
  c_guid, c_date, c_time, c_smalldatetime, c_datetime, c_datetime2, c_dto, c_variant, c_geography, c_hierarchyid)
VALUES (1, 1, 255, -32768, 2147483647, -9223372036854775808, 1234567890123456789012345678.1234567890, 9999999999,
  922337203685477.5807, -214748.3648, 0.1, 1.7976931348623157E+308, 'ab', 'varchar é', N'ñ', N'日本語 😀 Ünïcode',
  'legacy text', N'legacy ntext', N'<a x="1"><b>t</b></a>', 0x01020304, 0xDEADBEEF, 0x00FF,
  '6F9619FF-8B86-D011-B42D-00C04FC964FF', '9999-12-31', '23:59:59.9999999', '2079-06-06 23:59:00', '1753-01-01 00:00:00.003',
  '0001-01-01 00:00:00.0000001', '2024-05-31 13:45:00.1234567 +07:00', CAST(N'variant text' AS SQL_VARIANT),
  geography::Point(47.65, -122.36, 4326), hierarchyid::Parse('/1/2/'));
INSERT INTO dbo.types_all (id) VALUES (2);
