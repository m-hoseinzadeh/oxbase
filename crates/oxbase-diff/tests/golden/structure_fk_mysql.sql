ALTER TABLE `m8_tgt`.`orders` DROP FOREIGN KEY `orders_customer_fk`;
ALTER TABLE `m8_tgt`.`customers`
  MODIFY COLUMN `id` bigint NOT NULL;
ALTER TABLE `m8_tgt`.`orders`
  MODIFY COLUMN `customer_id` bigint NOT NULL;
ALTER TABLE `m8_tgt`.`orders` ADD CONSTRAINT `orders_customer_fk` FOREIGN KEY (`customer_id`) REFERENCES `m8_tgt`.`customers` (`id`) ON DELETE NO ACTION ON UPDATE NO ACTION;
