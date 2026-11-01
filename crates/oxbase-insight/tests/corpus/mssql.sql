-- Valid SQL Server (T-SQL) batches: diagnostics must report no error and no warning for any
-- of them, without a schema cache and with the sample cache of tests/diagnose.rs.

SELECT 1;
GO
SELECT 1 AS one, 'two' AS two, N'three' AS three;
GO
SELECT * FROM customers;
GO
SELECT * FROM dbo.customers;
GO
SELECT [id], [name] FROM [dbo].[customers] WHERE [country] = 'DE';
GO
SELECT "id", "name" FROM "customers";
GO
SELECT id, name, email FROM customers WHERE country = N'FR';
GO
SELECT c.id, c.name FROM customers c WHERE c.status = 'active' ORDER BY c.name;
GO
SELECT c.id FROM dbo.customers AS c ORDER BY c.id DESC;
GO
SELECT id FROM shop.dbo.customers;
GO
SELECT DISTINCT country FROM customers;
GO
SELECT TOP 10 id, total FROM orders ORDER BY total DESC;
GO
SELECT TOP (10) id FROM orders ORDER BY id;
GO
SELECT TOP (5) PERCENT id FROM orders ORDER BY total;
GO
SELECT TOP 3 WITH TIES id, total FROM orders ORDER BY total DESC;
GO
SELECT count(*) FROM orders;
GO
SELECT COUNT(DISTINCT customer_id) FROM orders;
GO
SELECT status, COUNT(*) AS n FROM orders GROUP BY status HAVING COUNT(*) > 10;
GO
SELECT customer_id, SUM(total) AS spent FROM orders GROUP BY customer_id ORDER BY spent DESC;
GO
SELECT id FROM orders ORDER BY id OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY;
GO
SELECT id FROM orders ORDER BY id OFFSET 0 ROWS;
GO
SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id;
GO
SELECT o.id, c.name FROM orders o INNER JOIN customers c ON c.id = o.customer_id WHERE o.total > 100;
GO
SELECT o.id, c.name FROM orders o LEFT JOIN customers c ON c.id = o.customer_id;
GO
SELECT o.id, c.name FROM orders o RIGHT OUTER JOIN customers c ON c.id = o.customer_id;
GO
SELECT o.id, c.name FROM orders o FULL JOIN customers c ON c.id = o.customer_id;
GO
SELECT c.name, p.name FROM customers c CROSS JOIN products p;
GO
SELECT o.id, i.quantity FROM orders o JOIN order_items i ON i.order_id = o.id JOIN products p ON p.id = i.product_id;
GO
SELECT c.name, x.total FROM customers c CROSS APPLY (SELECT TOP 1 o.total FROM orders o WHERE o.customer_id = c.id ORDER BY o.order_date DESC) x;
GO
SELECT c.name, x.total FROM customers c OUTER APPLY (SELECT SUM(o.total) AS total FROM orders o WHERE o.customer_id = c.id) AS x;
GO
SELECT t.n FROM (SELECT id AS n FROM customers) AS t WHERE t.n > 5;
GO
SELECT n FROM (SELECT id AS n FROM customers) t;
GO
SELECT v.a, v.b FROM (VALUES (1, 'x'), (2, 'y')) AS v(a, b);
GO
SELECT id FROM customers WHERE id IN (SELECT customer_id FROM orders WHERE total > 500);
GO
SELECT id FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id);
GO
SELECT name, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) AS order_count FROM customers c;
GO
SELECT id FROM orders WHERE total > (SELECT AVG(total) FROM orders);
GO
SELECT id FROM orders WHERE id = @id AND status = @status;
GO
SELECT id FROM orders WHERE order_date BETWEEN '2024-01-01' AND '2024-12-31';
GO
SELECT id FROM orders WHERE order_date >= DATEADD(day, -30, GETDATE());
GO
SELECT DATEDIFF(day, order_date, shipped_at) AS days, DATEPART(year, order_date) AS y FROM orders;
GO
SELECT FORMAT(order_date, 'yyyy-MM') FROM orders;
GO
SELECT CONVERT(varchar(10), order_date, 120), CAST(total AS decimal(10, 2)), TRY_CAST(notes AS int) FROM orders;
GO
SELECT TRY_CONVERT(int, notes) FROM orders;
GO
SELECT id FROM orders WHERE shipped_at IS NULL;
GO
SELECT id FROM customers WHERE name LIKE 'A%' AND email NOT LIKE '%@test%';
GO
SELECT id FROM customers WHERE country IN ('DE', 'FR');
GO
SELECT CASE WHEN total > 100 THEN 'big' ELSE 'small' END AS size FROM orders;
GO
SELECT IIF(total > 100, 'big', 'small') FROM orders;
GO
SELECT ISNULL(notes, ''), COALESCE(notes, 'none'), NULLIF(total, 0) FROM orders;
GO
SELECT UPPER(name), LOWER(email), LEN(name), SUBSTRING(name, 1, 3), LEFT(name, 2), RIGHT(name, 2) FROM customers;
GO
SELECT REPLACE(email, '@', ' at '), CHARINDEX('a', name), LTRIM(RTRIM(name)) FROM customers;
GO
SELECT name + ' <' + email + '>' AS contact FROM customers;
GO
SELECT CONCAT_WS(', ', city, country) FROM customers;
GO
SELECT STRING_AGG(name, ', ') WITHIN GROUP (ORDER BY name) FROM customers;
GO
SELECT ROUND(AVG(total), 2), MIN(total), MAX(total) FROM orders;
GO
SELECT id, ROW_NUMBER() OVER (ORDER BY total DESC) AS rn FROM orders;
GO
SELECT id, RANK() OVER (PARTITION BY customer_id ORDER BY total DESC) AS r FROM orders;
GO
SELECT id, SUM(total) OVER (PARTITION BY customer_id ORDER BY order_date ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM orders;
GO
SELECT JSON_VALUE(payload, '$.action') FROM audit_log;
GO
SELECT j.[key], j.[value] FROM audit_log a CROSS APPLY OPENJSON(a.payload) j;
GO
SELECT id FROM orders WITH (NOLOCK) WHERE status = 'open';
GO
SELECT o.id FROM orders AS o WITH (READPAST) WHERE o.total > 0;
GO
SELECT id FROM orders OPTION (RECOMPILE);
GO
SELECT id, total FROM orders FOR JSON PATH;
GO
SELECT id FROM orders FOR XML RAW;
GO
SELECT id FROM customers UNION SELECT customer_id FROM orders;
GO
SELECT id FROM customers UNION ALL SELECT customer_id FROM orders ORDER BY 1;
GO
SELECT id FROM customers INTERSECT SELECT customer_id FROM orders;
GO
SELECT id FROM customers EXCEPT SELECT customer_id FROM orders;
GO
SELECT name FROM customers UNION SELECT name FROM products ORDER BY name;
GO
WITH recent AS (SELECT * FROM orders WHERE order_date > DATEADD(day, -7, GETDATE())) SELECT COUNT(*) FROM recent;
GO
WITH recent AS (SELECT id, customer_id FROM orders) SELECT r.id, c.name FROM recent r JOIN customers c ON c.id = r.customer_id;
GO
WITH totals (customer_id, spent) AS (SELECT customer_id, SUM(total) FROM orders GROUP BY customer_id) SELECT customer_id, spent FROM totals;
GO
WITH tree AS (SELECT id, parent_id FROM categories WHERE parent_id IS NULL UNION ALL SELECT c.id, c.parent_id FROM categories c JOIN tree t ON c.parent_id = t.id) SELECT id FROM tree OPTION (MAXRECURSION 100);
GO
SELECT e.first_name, m.first_name AS manager FROM employees e LEFT JOIN employees m ON m.id = e.manager_id;
GO
SELECT d.name, COUNT(e.id) FROM departments d LEFT JOIN employees e ON e.department_id = d.id GROUP BY d.name;
GO
SELECT * FROM sales.regions;
GO
SELECT r.name, t.amount FROM sales.regions r JOIN sales.targets t ON t.region_id = r.id WHERE t.year = 2024;
GO
SELECT customer_id, order_count FROM v_customer_totals WHERE total_amount > 0;
GO
SELECT dbo.fn_order_total(id) FROM orders;
GO
SELECT name, object_id FROM sys.objects WHERE type = 'U';
GO
SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES;
GO
SELECT @@ROWCOUNT, @@VERSION, SYSDATETIME(), CURRENT_TIMESTAMP, SYSTEM_USER;
GO
SELECT NEXT VALUE FOR dbo.invoice_numbers;
GO
SELECT 'it''s', N'unicode', 0x1F;
GO
SELECT /* block /* nested */ comment */ 1;
GO
SELECT x = id, label = name FROM customers;
GO
SELECT p.region, p.[2023], p.[2024] FROM (SELECT region_id AS region, year, amount FROM sales.targets) s PIVOT (SUM(amount) FOR year IN ([2023], [2024])) p;
GO
SELECT id INTO #tmp_ids FROM orders WHERE status = 'open';
GO
SELECT id FROM #tmp_ids;
GO
DECLARE @n int = 0;
GO
DECLARE @t TABLE (id int PRIMARY KEY, label nvarchar(40));
GO
INSERT INTO customers (name, email) VALUES ('Ann', 'ann@example.com');
GO
INSERT INTO customers (name, email) VALUES (N'Bo', 'bo@example.com'), (N'Cy', 'cy@example.com');
GO
INSERT INTO customers (name, email) OUTPUT inserted.id VALUES ('Dee', 'dee@example.com');
GO
INSERT customers (name) VALUES ('Eve');
GO
INSERT INTO orders (customer_id, total) SELECT id, 0 FROM customers WHERE status = 'new';
GO
INSERT INTO audit_log DEFAULT VALUES;
GO
INSERT INTO audit_log (table_name, action) EXEC dbo.usp_archive_orders 30;
GO
UPDATE customers SET status = 'inactive' WHERE created_at < DATEADD(year, -2, GETDATE());
GO
UPDATE c SET c.credit_limit = c.credit_limit * 1.1 FROM customers c WHERE c.country = 'DE';
GO
UPDATE o SET o.status = 'vip' FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.status = 'vip';
GO
UPDATE TOP (100) audit_log SET payload = NULL WHERE changed_at < '2020-01-01';
GO
UPDATE orders SET total = 0 OUTPUT deleted.total, inserted.total WHERE id = 7;
GO
UPDATE orders SET status = 'shipped' FROM orders JOIN invoices ON invoices.order_id = orders.id WHERE invoices.paid_at IS NOT NULL;
GO
DELETE FROM orders WHERE id = 1;
GO
DELETE orders WHERE id = 2;
GO
DELETE TOP (1000) FROM audit_log WHERE changed_at < '2020-01-01';
GO
DELETE o FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.status = 'deleted';
GO
DELETE FROM order_items OUTPUT deleted.id WHERE order_id = 3;
GO
MERGE INTO customers AS t USING (SELECT 1 AS id, N'x' AS name) AS s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.name = s.name WHEN NOT MATCHED BY TARGET THEN INSERT (id, name) VALUES (s.id, s.name) OUTPUT $action, inserted.id;
GO
TRUNCATE TABLE audit_log;
GO
CREATE TABLE tmp_events (id bigint IDENTITY(1, 1) PRIMARY KEY, kind nvarchar(40) NOT NULL, created_at datetime2 DEFAULT SYSDATETIME(), customer_id int REFERENCES customers (id));
GO
INSERT INTO tmp_events (kind) VALUES ('created');
GO
SELECT kind, created_at FROM tmp_events WHERE customer_id IS NULL;
GO
CREATE TABLE #work (id int, total decimal(10, 2));
GO
INSERT INTO #work (id, total) SELECT id, total FROM orders;
GO
SELECT w.id FROM #work w JOIN orders o ON o.id = w.id;
GO
CREATE VIEW v_open_orders AS SELECT id, customer_id, total FROM orders WHERE status = 'open';
GO
CREATE OR ALTER VIEW dbo.v_big_orders WITH SCHEMABINDING AS SELECT id, total FROM dbo.orders WHERE total > 1000;
GO
CREATE INDEX idx_orders_customer ON orders (customer_id) INCLUDE (total);
GO
CREATE UNIQUE NONCLUSTERED INDEX idx_customers_email ON dbo.customers (email) WHERE email IS NOT NULL;
GO
CREATE SCHEMA reporting;
GO
CREATE TABLE reporting.daily_totals (day date PRIMARY KEY, total decimal(12, 2));
GO
SELECT day, total FROM reporting.daily_totals;
GO
CREATE SEQUENCE dbo.invoice_numbers START WITH 1000 INCREMENT BY 1;
GO
ALTER TABLE customers ADD phone nvarchar(30) NULL;
GO
ALTER TABLE orders ADD CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id) REFERENCES customers (id);
GO
ALTER TABLE products ALTER COLUMN price decimal(10, 2) NOT NULL;
GO
DROP TABLE IF EXISTS tmp_events;
GO
DROP VIEW IF EXISTS v_open_orders;
GO
IF OBJECT_ID('dbo.tmp_x', 'U') IS NOT NULL DROP TABLE dbo.tmp_x;
GO
EXEC dbo.usp_archive_orders @days = 30;
GO
EXECUTE ('SELECT 1');
GO
PRINT 'done';
GO
RAISERROR ('warning %d', 10, 1, 5);
GO
THROW 50001, 'custom error', 1;
GO
BEGIN TRANSACTION;
GO
SAVE TRANSACTION s1;
GO
ROLLBACK TRANSACTION s1;
GO
COMMIT TRANSACTION;
GO
SET NOCOUNT ON;
GO
SET XACT_ABORT ON;
GO
SET TRANSACTION ISOLATION LEVEL READ COMMITTED;
GO
USE shop;
GO
WAITFOR DELAY '00:00:01';
GO
GRANT SELECT ON dbo.customers TO report_role;
GO
DENY DELETE ON dbo.orders TO report_role;
GO
DECLARE @cnt int;
SELECT @cnt = COUNT(*) FROM orders WHERE status = 'open';
IF @cnt > 100
BEGIN
    UPDATE orders SET status = 'review' WHERE status = 'open' AND total > 1000;
    PRINT 'flagged';
END
ELSE
    SELECT @cnt AS open_orders;
GO
BEGIN TRY
    INSERT INTO audit_log (table_name, action) VALUES ('orders', 'check');
    SELECT TOP 1 id FROM orders ORDER BY id DESC
END TRY
BEGIN CATCH
    SELECT ERROR_MESSAGE() AS msg
END CATCH
GO
DECLARE @i int = 0
WHILE @i < 3
BEGIN
    SET @i = @i + 1
    UPDATE products SET stock = stock + 1 WHERE id = @i
END
GO
SELECT id FROM customers WHERE status = 'new'
SELECT id FROM orders WHERE status = 'open'
UPDATE orders SET total = 0 WHERE id = -1
GO
CREATE PROCEDURE dbo.usp_purge @days int = 90
AS
BEGIN
    SET NOCOUNT ON;
    DELETE FROM audit_log WHERE changed_at < DATEADD(day, -@days, GETDATE());
    SELECT @@ROWCOUNT AS purged;
END
GO
CREATE OR ALTER FUNCTION dbo.fn_customer_orders (@cid int)
RETURNS TABLE
AS
RETURN (SELECT id, total FROM dbo.orders WHERE customer_id = @cid)
GO
CREATE FUNCTION dbo.fn_double (@x int) RETURNS int
AS
BEGIN
    RETURN @x * 2
END
GO
CREATE TRIGGER trg_orders_audit ON dbo.orders AFTER INSERT, UPDATE
AS
BEGIN
    INSERT INTO audit_log (table_name, action) SELECT 'orders', 'change' FROM inserted i JOIN deleted d ON d.id = i.id;
END
GO
SELECT * FROM dbo.fn_customer_orders(1)
GO
DECLARE c CURSOR FOR SELECT id FROM orders WHERE status = 'open';
OPEN c;
FETCH NEXT FROM c INTO @id;
CLOSE c;
DEALLOCATE c;
GO
;WITH latest AS (SELECT customer_id, MAX(order_date) AS last_order FROM orders GROUP BY customer_id)
SELECT c.name, l.last_order FROM customers c JOIN latest l ON l.customer_id = c.id
GO
EXEC sp_rename 'dbo.tmp_like', 'tmp_like_old';
GO
