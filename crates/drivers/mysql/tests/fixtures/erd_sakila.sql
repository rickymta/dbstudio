-- Hand-written Sakila-style fixture for the ERD / DDL integration tests (23 tables, 3 views).
-- Covers: optional FKs, 1-1 relations (PK and unique), composite keys and composite FKs, self
-- references, an FK cycle between two tables (store <-> staff), plain/unique/fulltext indexes,
-- enum/set columns, comments, a partitioned table, views (one on a view), a function, a
-- procedure and triggers.

CREATE TABLE country (
  country_id int unsigned NOT NULL AUTO_INCREMENT,
  country varchar(50) NOT NULL COMMENT 'Country name; it''s required',
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (country_id)
) ENGINE=InnoDB COMMENT='Countries';
CREATE TABLE city (
  city_id int unsigned NOT NULL AUTO_INCREMENT,
  city varchar(50) NOT NULL,
  country_id int unsigned NOT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (city_id),
  KEY idx_fk_country_id (country_id),
  CONSTRAINT fk_city_country FOREIGN KEY (country_id) REFERENCES country (country_id) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB;
CREATE TABLE address (
  address_id int unsigned NOT NULL AUTO_INCREMENT,
  address varchar(50) NOT NULL,
  address2 varchar(50) DEFAULT NULL,
  district varchar(20) NOT NULL,
  city_id int unsigned NOT NULL,
  postal_code varchar(10) DEFAULT NULL,
  phone varchar(20) NOT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (address_id),
  KEY idx_fk_city_id (city_id),
  CONSTRAINT fk_address_city FOREIGN KEY (city_id) REFERENCES city (city_id) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB;
CREATE TABLE language (
  language_id int unsigned NOT NULL AUTO_INCREMENT,
  name char(20) NOT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (language_id)
) ENGINE=InnoDB;
CREATE TABLE category (
  category_id int unsigned NOT NULL AUTO_INCREMENT,
  name varchar(25) NOT NULL,
  parent_category_id int unsigned DEFAULT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (category_id),
  KEY idx_parent (parent_category_id),
  CONSTRAINT fk_category_parent FOREIGN KEY (parent_category_id) REFERENCES category (category_id) ON DELETE SET NULL
) ENGINE=InnoDB;
CREATE TABLE actor (
  actor_id int unsigned NOT NULL AUTO_INCREMENT,
  first_name varchar(45) NOT NULL,
  last_name varchar(45) NOT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (actor_id),
  UNIQUE KEY uq_actor_name (first_name, last_name),
  KEY idx_actor_last_name (last_name)
) ENGINE=InnoDB;
CREATE TABLE film (
  film_id int unsigned NOT NULL AUTO_INCREMENT,
  title varchar(255) NOT NULL,
  description text,
  release_year smallint DEFAULT NULL,
  language_id int unsigned NOT NULL,
  original_language_id int unsigned DEFAULT NULL,
  rental_duration tinyint unsigned NOT NULL DEFAULT '3',
  rental_rate decimal(4,2) NOT NULL DEFAULT '4.99',
  length smallint unsigned DEFAULT NULL,
  replacement_cost decimal(5,2) NOT NULL DEFAULT '19.99',
  rating enum('G','PG','PG-13','R','NC-17') DEFAULT 'G' COMMENT 'MPAA rating',
  special_features set('Trailers','Commentaries','Deleted Scenes','Behind the Scenes') DEFAULT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (film_id),
  KEY idx_title (title),
  KEY idx_fk_language_id (language_id),
  KEY idx_fk_original_language_id (original_language_id),
  FULLTEXT KEY ft_description (description),
  CONSTRAINT fk_film_language FOREIGN KEY (language_id) REFERENCES language (language_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_film_language_original FOREIGN KEY (original_language_id) REFERENCES language (language_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT film_duration_chk CHECK (rental_duration > 0)
) ENGINE=InnoDB COMMENT='Films; one row per title';
CREATE TABLE film_actor (
  actor_id int unsigned NOT NULL,
  film_id int unsigned NOT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (actor_id, film_id),
  KEY idx_fk_film_id (film_id),
  CONSTRAINT fk_film_actor_actor FOREIGN KEY (actor_id) REFERENCES actor (actor_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_film_actor_film FOREIGN KEY (film_id) REFERENCES film (film_id) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB;
CREATE TABLE film_category (
  film_id int unsigned NOT NULL,
  category_id int unsigned NOT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (film_id, category_id),
  KEY fk_film_category_category (category_id),
  CONSTRAINT fk_film_category_category FOREIGN KEY (category_id) REFERENCES category (category_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_film_category_film FOREIGN KEY (film_id) REFERENCES film (film_id) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB;
CREATE TABLE film_text (
  film_id int unsigned NOT NULL,
  title varchar(255) NOT NULL,
  description text,
  PRIMARY KEY (film_id),
  CONSTRAINT fk_film_text_film FOREIGN KEY (film_id) REFERENCES film (film_id) ON DELETE CASCADE
) ENGINE=InnoDB;
CREATE TABLE store (
  store_id int unsigned NOT NULL AUTO_INCREMENT,
  manager_staff_id int unsigned NOT NULL,
  address_id int unsigned NOT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (store_id),
  UNIQUE KEY idx_unique_manager (manager_staff_id),
  KEY idx_fk_address_id (address_id),
  CONSTRAINT fk_store_address FOREIGN KEY (address_id) REFERENCES address (address_id) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB;
CREATE TABLE staff (
  staff_id int unsigned NOT NULL AUTO_INCREMENT,
  first_name varchar(45) NOT NULL,
  last_name varchar(45) NOT NULL,
  address_id int unsigned NOT NULL,
  email varchar(50) DEFAULT NULL,
  store_id int unsigned NOT NULL,
  reports_to int unsigned DEFAULT NULL,
  active tinyint(1) NOT NULL DEFAULT '1',
  username varchar(16) NOT NULL,
  picture blob,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (staff_id),
  KEY idx_fk_store_id (store_id),
  KEY idx_fk_address_id (address_id),
  KEY idx_reports_to (reports_to),
  CONSTRAINT fk_staff_address FOREIGN KEY (address_id) REFERENCES address (address_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_staff_store FOREIGN KEY (store_id) REFERENCES store (store_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_staff_reports_to FOREIGN KEY (reports_to) REFERENCES staff (staff_id)
) ENGINE=InnoDB;
ALTER TABLE store ADD CONSTRAINT fk_store_staff FOREIGN KEY (manager_staff_id) REFERENCES staff (staff_id) ON DELETE RESTRICT ON UPDATE CASCADE;
CREATE TABLE customer (
  customer_id int unsigned NOT NULL AUTO_INCREMENT,
  store_id int unsigned NOT NULL,
  first_name varchar(45) NOT NULL,
  last_name varchar(45) NOT NULL,
  email varchar(50) DEFAULT NULL COMMENT 'Contact e-mail',
  address_id int unsigned NOT NULL,
  active tinyint(1) NOT NULL DEFAULT '1',
  create_date datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_update timestamp NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (customer_id),
  KEY idx_fk_store_id (store_id),
  KEY idx_fk_address_id (address_id),
  KEY idx_last_name (last_name),
  CONSTRAINT fk_customer_address FOREIGN KEY (address_id) REFERENCES address (address_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_customer_store FOREIGN KEY (store_id) REFERENCES store (store_id) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB;
CREATE TABLE customer_profile (
  customer_id int unsigned NOT NULL,
  bio text,
  PRIMARY KEY (customer_id),
  CONSTRAINT fk_profile_customer FOREIGN KEY (customer_id) REFERENCES customer (customer_id) ON DELETE CASCADE
) ENGINE=InnoDB;
CREATE TABLE staff_credentials (
  credential_id int unsigned NOT NULL AUTO_INCREMENT,
  staff_id int unsigned NOT NULL,
  password_hash varchar(255) NOT NULL,
  PRIMARY KEY (credential_id),
  UNIQUE KEY uq_credentials_staff (staff_id),
  CONSTRAINT fk_credentials_staff FOREIGN KEY (staff_id) REFERENCES staff (staff_id)
) ENGINE=InnoDB;
CREATE TABLE inventory (
  inventory_id int unsigned NOT NULL AUTO_INCREMENT,
  film_id int unsigned NOT NULL,
  store_id int unsigned NOT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (inventory_id),
  KEY idx_fk_film_id (film_id),
  KEY idx_store_id_film_id (store_id, film_id),
  CONSTRAINT fk_inventory_film FOREIGN KEY (film_id) REFERENCES film (film_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_inventory_store FOREIGN KEY (store_id) REFERENCES store (store_id) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB;
CREATE TABLE rental (
  rental_id int NOT NULL AUTO_INCREMENT,
  rental_date datetime NOT NULL,
  inventory_id int unsigned NOT NULL,
  customer_id int unsigned NOT NULL,
  return_date datetime DEFAULT NULL,
  staff_id int unsigned NOT NULL,
  last_update timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (rental_id),
  UNIQUE KEY rental_date (rental_date, inventory_id, customer_id),
  KEY idx_fk_inventory_id (inventory_id),
  KEY idx_fk_customer_id (customer_id),
  KEY idx_fk_staff_id (staff_id),
  CONSTRAINT fk_rental_customer FOREIGN KEY (customer_id) REFERENCES customer (customer_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_rental_inventory FOREIGN KEY (inventory_id) REFERENCES inventory (inventory_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_rental_staff FOREIGN KEY (staff_id) REFERENCES staff (staff_id) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB;
CREATE TABLE payment (
  payment_id int unsigned NOT NULL AUTO_INCREMENT,
  customer_id int unsigned NOT NULL,
  staff_id int unsigned NOT NULL,
  rental_id int DEFAULT NULL,
  amount decimal(5,2) NOT NULL,
  payment_date datetime NOT NULL,
  PRIMARY KEY (payment_id),
  KEY idx_fk_staff_id (staff_id),
  KEY idx_fk_customer_id (customer_id),
  KEY fk_payment_rental (rental_id),
  CONSTRAINT fk_payment_customer FOREIGN KEY (customer_id) REFERENCES customer (customer_id) ON DELETE RESTRICT ON UPDATE CASCADE,
  CONSTRAINT fk_payment_rental FOREIGN KEY (rental_id) REFERENCES rental (rental_id) ON DELETE SET NULL ON UPDATE CASCADE,
  CONSTRAINT fk_payment_staff FOREIGN KEY (staff_id) REFERENCES staff (staff_id) ON DELETE RESTRICT ON UPDATE CASCADE
) ENGINE=InnoDB;
CREATE TABLE rental_item (
  rental_id int NOT NULL,
  line_no int NOT NULL,
  qty int NOT NULL DEFAULT '1',
  PRIMARY KEY (rental_id, line_no),
  CONSTRAINT fk_item_rental FOREIGN KEY (rental_id) REFERENCES rental (rental_id) ON DELETE CASCADE
) ENGINE=InnoDB;
CREATE TABLE rental_item_note (
  note_id int NOT NULL AUTO_INCREMENT,
  rental_id int NOT NULL,
  line_no int NOT NULL,
  note text,
  PRIMARY KEY (note_id),
  KEY idx_item (rental_id, line_no),
  CONSTRAINT fk_note_item FOREIGN KEY (rental_id, line_no) REFERENCES rental_item (rental_id, line_no) ON DELETE CASCADE
) ENGINE=InnoDB;
CREATE TABLE film_review (
  review_id int NOT NULL AUTO_INCREMENT,
  film_id int unsigned NOT NULL,
  customer_id int unsigned DEFAULT NULL,
  stars tinyint NOT NULL,
  body text,
  PRIMARY KEY (review_id),
  KEY idx_review_film (film_id),
  KEY idx_review_customer (customer_id),
  CONSTRAINT fk_review_film FOREIGN KEY (film_id) REFERENCES film (film_id),
  CONSTRAINT fk_review_customer FOREIGN KEY (customer_id) REFERENCES customer (customer_id)
) ENGINE=InnoDB;
CREATE TABLE rental_archive (
  archive_id int NOT NULL,
  rental_date date NOT NULL,
  customer_id int DEFAULT NULL
) ENGINE=InnoDB
PARTITION BY RANGE (YEAR(rental_date)) (
  PARTITION p2023 VALUES LESS THAN (2024),
  PARTITION p2024 VALUES LESS THAN (2025),
  PARTITION pmax VALUES LESS THAN MAXVALUE
);
CREATE TABLE audit_log (
  audit_id bigint NOT NULL AUTO_INCREMENT,
  customer_id int unsigned DEFAULT NULL,
  note text,
  PRIMARY KEY (audit_id),
  KEY idx_audit_customer (customer_id),
  CONSTRAINT fk_audit_customer FOREIGN KEY (customer_id) REFERENCES customer (customer_id)
) ENGINE=InnoDB;

CREATE VIEW film_list AS
  SELECT f.film_id, f.title, c.name AS category
  FROM film f JOIN film_category fc ON fc.film_id = f.film_id JOIN category c ON c.category_id = fc.category_id;
CREATE VIEW customer_list AS
  SELECT cu.customer_id, CONCAT(cu.first_name, ' ', cu.last_name) AS name, a.address, ci.city
  FROM customer cu JOIN address a ON a.address_id = cu.address_id JOIN city ci ON ci.city_id = a.city_id;
CREATE VIEW film_list_long AS SELECT * FROM film_list WHERE CHAR_LENGTH(title) > 10;

CREATE FUNCTION film_stock(p_film int unsigned) RETURNS int READS SQL DATA
BEGIN
  DECLARE n int;
  SELECT COUNT(*) INTO n FROM inventory WHERE film_id = p_film;
  RETURN n;
END;
CREATE PROCEDURE touch_actor(IN p_id int unsigned)
BEGIN
  UPDATE actor SET first_name = first_name WHERE actor_id = p_id;
  SELECT ROW_COUNT();
END;
CREATE TRIGGER ins_film BEFORE INSERT ON film FOR EACH ROW SET NEW.last_update = NOW();
CREATE TRIGGER upd_actor BEFORE UPDATE ON actor FOR EACH ROW
BEGIN
  SET NEW.last_update = NOW();
  SET NEW.first_name = TRIM(NEW.first_name);
END;

INSERT INTO country (country) VALUES ('Vietnam'), ('Japan'), ('Peru');
INSERT INTO language (name) VALUES ('English'), ('Vietnamese');
INSERT INTO category (name, parent_category_id) VALUES ('Action', NULL), ('Drama', 1);
INSERT INTO film (title, language_id) VALUES ('Alpha', 1), ('Omega', 2);
INSERT INTO film_category (film_id, category_id) VALUES (1, 1), (2, 2);
INSERT INTO audit_log (note) VALUES ('first');
