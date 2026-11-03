DROP VIEW `m8_tgt`.`active_customers`;
ALTER TABLE `m8_tgt`.`orders` DROP FOREIGN KEY `orders_customer_fk`;
DROP TABLE `m8_tgt`.`order_lines`;
DROP TABLE `m8_tgt`.`legacy_notes`;
CREATE SEQUENCE `m8_tgt`.`invoice_seq` AS bigint START WITH 1000 MINVALUE 1 MAXVALUE 9999999 INCREMENT BY 1 NOCYCLE;
CREATE TABLE `m8_tgt`.`MixedCase` (
  `Id` int NOT NULL,
  `Label` varchar(20) NULL,
  PRIMARY KEY (`Id`)
);
CREATE TABLE `m8_tgt`.`order_audit` (
  `id` int NOT NULL,
  `order_id` int NOT NULL,
  `at` datetime NULL,
  PRIMARY KEY (`id`)
);
CREATE TABLE `m8_tgt`.`order_items` (
  `id` int NOT NULL,
  `order_id` int NOT NULL,
  `product_id` int NOT NULL,
  `qty` int NOT NULL,
  PRIMARY KEY (`id`)
);
ALTER TABLE `m8_tgt`.`customers`
  ADD COLUMN `email` varchar(200) NULL;
ALTER TABLE `m8_tgt`.`products`
  MODIFY COLUMN `price` decimal(10,2) NULL;
ALTER TABLE `m8_tgt`.`select`
  ADD COLUMN `order` int NULL;
ALTER TABLE `m8_tgt`.`order_items` ADD CONSTRAINT `order_items_order_fk` FOREIGN KEY (`order_id`) REFERENCES `m8_tgt`.`orders` (`id`) ON DELETE CASCADE ON UPDATE NO ACTION;
ALTER TABLE `m8_tgt`.`order_items` ADD CONSTRAINT `order_items_product_fk` FOREIGN KEY (`product_id`) REFERENCES `m8_tgt`.`products` (`id`) ON DELETE NO ACTION ON UPDATE NO ACTION;
ALTER TABLE `m8_tgt`.`orders` ADD CONSTRAINT `orders_customer_fk` FOREIGN KEY (`customer_id`) REFERENCES `m8_tgt`.`customers` (`id`) ON DELETE CASCADE ON UPDATE NO ACTION;
CREATE FUNCTION `m8_tgt`.`order_total`(`p_id` int) RETURNS decimal(12,2)
    READS SQL DATA
    SQL SECURITY DEFINER
RETURN (SELECT SUM(qty) FROM `m8_tgt`.`order_items` WHERE order_id = p_id);
CREATE VIEW `m8_tgt`.`active_customers` AS select `m8_tgt`.`customers`.`id` AS `id`,`m8_tgt`.`customers`.`name` AS `name`,`m8_tgt`.`customers`.`email` AS `email` from `m8_tgt`.`customers`;
CREATE TRIGGER `m8_tgt`.`orders_audit` AFTER INSERT ON `m8_tgt`.`orders` FOR EACH ROW INSERT INTO `m8_tgt`.`order_audit` (order_id) VALUES (NEW.id);
