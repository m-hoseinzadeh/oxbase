-- oxbase_test fixture rows (identical across engines).
INSERT INTO customers (email, name, created_at) VALUES
    ('alice@example.com', 'Alice Anders', '2026-01-15 10:00:00'),
    ('bob@example.com',   'Bob Brown',    '2026-01-16 11:30:00'),
    ('carol@example.com', 'Carol Chen',   '2026-01-17 09:15:00');

-- No sequence on MySQL: order_no is given explicitly (1000..1004, as on the other engines).
INSERT INTO orders (order_no, customer_id, status, total, ordered_at) VALUES
    (1000, 1, 'paid',      120.50, '2026-02-01 08:00:00'),
    (1001, 1, 'shipped',    35.00, '2026-02-03 14:20:00'),
    (1002, 2, 'new',       999.99, '2026-02-05 16:45:00'),
    (1003, 2, 'cancelled',   0.00, '2026-02-06 10:10:00'),
    (1004, 3, 'paid',       15.25, '2026-02-07 12:00:00');
