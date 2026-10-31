-- oxbase_test fixture rows (identical across engines).
USE oxbase_test;
GO
SET NOCOUNT ON;

INSERT INTO dbo.customers (email, name, created_at) VALUES
    ('alice@example.com', 'Alice Anders', '2026-01-15T10:00:00'),
    ('bob@example.com',   'Bob Brown',    '2026-01-16T11:30:00'),
    ('carol@example.com', 'Carol Chen',   '2026-01-17T09:15:00');

-- order_no comes from order_no_seq: 1000..1004.
INSERT INTO dbo.orders (customer_id, status, total, ordered_at) VALUES
    (1, 'paid',      120.50, '2026-02-01T08:00:00'),
    (1, 'shipped',    35.00, '2026-02-03T14:20:00'),
    (2, 'new',       999.99, '2026-02-05T16:45:00'),
    (2, 'cancelled',   0.00, '2026-02-06T10:10:00'),
    (3, 'paid',       15.25, '2026-02-07T12:00:00');
GO
