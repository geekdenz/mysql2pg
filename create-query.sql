CREATE TABLE IF NOT EXISTS order_details (
  order_id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  product_id INT UNSIGNED NOT NULL,
  customer_id INT UNSIGNED NOT NULL,
  quantity SMALLINT NOT NULL DEFAULT 1,
  price DECIMAL(10, 2) NOT NULL,
  discount DECIMAL(3, 2) DEFAULT 0.00,
  status ENUM('pending', 'shipped', 'delivered', 'cancelled') DEFAULT 'pending',
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (order_id),
  UNIQUE KEY unique_order_prod (order_id, product_id),
  CONSTRAINT fk_product FOREIGN KEY (product_id) REFERENCES products(id) ON DELETE CASCADE,
  CONSTRAINT chk_quantity CHECK (quantity > 0) ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

