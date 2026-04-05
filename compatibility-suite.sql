DROP VIEW IF EXISTS qa_customer_totals;
DROP TABLE IF EXISTS qa_order_items;
DROP TABLE IF EXISTS qa_orders;
DROP TABLE IF EXISTS qa_products;
DROP TABLE IF EXISTS qa_customers;

CREATE TABLE IF NOT EXISTS qa_customers (
  id INT UNSIGNED NOT NULL AUTO_INCREMENT,
  email VARCHAR(255) NOT NULL,
  full_name VARCHAR(255) NOT NULL,
  is_vip BOOLEAN NOT NULL DEFAULT FALSE,
  metadata JSON,
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY unique_customer_email (email)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS qa_products (
  id INT UNSIGNED NOT NULL AUTO_INCREMENT,
  sku VARCHAR(64) NOT NULL,
  title VARCHAR(255) NOT NULL,
  price DECIMAL(10, 2) NOT NULL,
  active BOOLEAN NOT NULL DEFAULT TRUE,
  PRIMARY KEY (id),
  UNIQUE KEY unique_product_sku (sku)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS qa_orders (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  customer_id INT UNSIGNED NOT NULL,
  status ENUM('pending', 'paid', 'shipped', 'cancelled') NOT NULL DEFAULT 'pending',
  placed_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  notes TEXT,
  discount DECIMAL(10, 2) DEFAULT 0.00,
  PRIMARY KEY (id),
  CONSTRAINT fk_qa_orders_customer FOREIGN KEY (customer_id) REFERENCES qa_customers(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS qa_order_items (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  order_id BIGINT UNSIGNED NOT NULL,
  product_id INT UNSIGNED NOT NULL,
  quantity SMALLINT UNSIGNED NOT NULL DEFAULT 1,
  unit_price DECIMAL(10, 2) NOT NULL,
  attributes JSON,
  PRIMARY KEY (id),
  UNIQUE KEY unique_order_product (order_id, product_id),
  CONSTRAINT fk_qa_items_order FOREIGN KEY (order_id) REFERENCES qa_orders(id) ON DELETE CASCADE,
  CONSTRAINT fk_qa_items_product FOREIGN KEY (product_id) REFERENCES qa_products(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

INSERT INTO qa_customers (email, full_name, is_vip, metadata) VALUES
  ('ada@example.com', 'Ada Lovelace', TRUE, '{"tier":"gold","region":"emea"}'),
  ('grace@example.com', 'Grace Hopper', FALSE, '{"tier":"silver","region":"us"}'),
  ('linus@example.com', 'Linus Torvalds', TRUE, '{"tier":"platinum","region":"apac"}');

INSERT INTO qa_products (sku, title, price, active) VALUES
  ('LAP-13', 'Laptop 13"', 1299.00, TRUE),
  ('MOU-01', 'Wireless Mouse', 49.50, TRUE),
  ('DOC-USB', 'USB-C Dock', 199.99, TRUE),
  ('CAB-HDMI', 'HDMI Cable', 15.25, FALSE);

INSERT INTO qa_orders (customer_id, status, notes, discount) VALUES
  (1, 'paid', 'rush shipping requested', 25.00),
  (1, 'shipped', NULL, 0.00),
  (2, 'pending', 'awaiting approval', 10.00),
  (3, 'paid', 'gift wrap', 5.50);

INSERT INTO qa_order_items (order_id, product_id, quantity, unit_price, attributes) VALUES
  (1, 1, 1, 1299.00, '{"color":"silver","warranty_years":2}'),
  (1, 2, 2, 49.50, '{"color":"black","wireless":true}'),
  (2, 3, 1, 199.99, '{"ports":8}'),
  (3, 4, 3, 15.25, '{"length_m":2}'),
  (4, 2, 1, 49.50, '{"color":"white","wireless":true}'),
  (4, 3, 1, 199.99, '{"ports":8}');

CREATE VIEW qa_customer_totals AS
SELECT
  c.id AS customer_id,
  c.full_name,
  COUNT(DISTINCT o.id) AS order_count,
  ROUND(COALESCE(SUM(oi.quantity * oi.unit_price) - SUM(o.discount), 0), 2) AS gross_total
FROM qa_customers c
LEFT JOIN qa_orders o ON o.customer_id = c.id
LEFT JOIN qa_order_items oi ON oi.order_id = o.id
GROUP BY c.id, c.full_name;
