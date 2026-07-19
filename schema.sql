CREATE TABLE IF NOT EXISTS orders (
    id BIGSERIAL PRIMARY KEY,
    customer_id BIGINT NOT NULL,
    product_id BIGINT NOT NULL,
    quantity INT NOT NULL,
    total_cents BIGINT NOT NULL
);

-- Несколько тестовых записей (опционально)
INSERT INTO orders (customer_id, product_id, quantity, total_cents) VALUES
    (1, 101, 2, 1999),
    (2, 102, 1, 499),
    (3, 103, 5, 9995)
ON CONFLICT DO NOTHING;
