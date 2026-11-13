-- Hand-written Pagila-style fixture for the ERD / DDL integration tests (about 24 tables).
-- Covers: optional FKs, 1-1 relations, composite keys and composite FKs, self references, an FK
-- cycle between two tables (store <-> staff), serial and identity columns, a standalone sequence,
-- enum/domain types, check constraints, partial/expression/gin indexes, views, a materialized
-- view, functions, triggers, partitions, a second schema and comments.

CREATE TYPE mpaa_rating AS ENUM ('G', 'PG', 'PG-13', 'R', 'NC-17');
CREATE DOMAIN year AS integer CHECK (VALUE >= 1901 AND VALUE <= 2155);

CREATE SEQUENCE payment_group_seq START WITH 100 INCREMENT BY 5;

CREATE FUNCTION last_updated() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.last_update := CURRENT_TIMESTAMP;
    RETURN NEW;
END $$;

CREATE TABLE country (
    country_id serial PRIMARY KEY,
    country varchar(50) NOT NULL,
    last_update timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE city (
    city_id serial PRIMARY KEY,
    city varchar(50) NOT NULL,
    country_id integer NOT NULL REFERENCES country (country_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    last_update timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE address (
    address_id serial PRIMARY KEY,
    address varchar(50) NOT NULL,
    address2 varchar(50),
    district varchar(20) NOT NULL,
    city_id integer NOT NULL REFERENCES city (city_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    postal_code varchar(10),
    phone varchar(20) NOT NULL,
    last_update timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE language (
    language_id serial PRIMARY KEY,
    name char(20) NOT NULL,
    last_update timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE category (
    category_id serial PRIMARY KEY,
    name varchar(25) NOT NULL,
    parent_category_id integer REFERENCES category (category_id) ON DELETE SET NULL,
    last_update timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE actor (
    actor_id serial PRIMARY KEY,
    first_name varchar(45) NOT NULL,
    last_name varchar(45) NOT NULL,
    last_update timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE film (
    film_id serial PRIMARY KEY,
    title varchar(255) NOT NULL,
    description text,
    release_year year,
    language_id integer NOT NULL REFERENCES language (language_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    original_language_id integer REFERENCES language (language_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    rental_duration smallint NOT NULL DEFAULT 3 CHECK (rental_duration > 0),
    rental_rate numeric(4,2) NOT NULL DEFAULT 4.99,
    length smallint,
    replacement_cost numeric(5,2) NOT NULL DEFAULT 19.99,
    rating mpaa_rating DEFAULT 'G',
    special_features text[],
    fulltext tsvector NOT NULL DEFAULT ''::tsvector,
    last_update timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT film_length_chk CHECK (length IS NULL OR length > 0)
);
CREATE TABLE film_actor (
    actor_id integer NOT NULL REFERENCES actor (actor_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    film_id integer NOT NULL REFERENCES film (film_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    last_update timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (actor_id, film_id)
);
CREATE TABLE film_category (
    film_id integer NOT NULL REFERENCES film (film_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    category_id integer NOT NULL REFERENCES category (category_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    last_update timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (film_id, category_id)
);
-- 1-1 through the primary key.
CREATE TABLE film_text (
    film_id integer PRIMARY KEY REFERENCES film (film_id) ON DELETE CASCADE,
    title text NOT NULL,
    description text
);
-- store <-> staff form an FK cycle.
CREATE TABLE store (
    store_id serial PRIMARY KEY,
    manager_staff_id integer NOT NULL UNIQUE,
    address_id integer NOT NULL REFERENCES address (address_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    last_update timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE staff (
    staff_id serial PRIMARY KEY,
    first_name varchar(45) NOT NULL,
    last_name varchar(45) NOT NULL,
    address_id integer NOT NULL REFERENCES address (address_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    email varchar(50),
    store_id integer NOT NULL REFERENCES store (store_id),
    reports_to integer REFERENCES staff (staff_id),
    active boolean NOT NULL DEFAULT true,
    username varchar(16) NOT NULL,
    picture bytea,
    last_update timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE store ADD CONSTRAINT store_manager_staff_id_fkey
    FOREIGN KEY (manager_staff_id) REFERENCES staff (staff_id) ON UPDATE CASCADE ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;
CREATE TABLE customer (
    customer_id serial PRIMARY KEY,
    store_id integer NOT NULL REFERENCES store (store_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    first_name varchar(45) NOT NULL,
    last_name varchar(45) NOT NULL,
    email varchar(50),
    address_id integer NOT NULL REFERENCES address (address_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    active boolean NOT NULL DEFAULT true,
    create_date date NOT NULL DEFAULT CURRENT_DATE,
    last_update timestamptz DEFAULT now()
);
-- 1-1 through a unique constraint.
CREATE TABLE customer_profile (
    customer_id integer PRIMARY KEY REFERENCES customer (customer_id) ON DELETE CASCADE,
    bio text
);
CREATE TABLE staff_credentials (
    credential_id serial PRIMARY KEY,
    staff_id integer NOT NULL UNIQUE REFERENCES staff (staff_id),
    password_hash text NOT NULL
);
CREATE TABLE inventory (
    inventory_id serial PRIMARY KEY,
    film_id integer NOT NULL REFERENCES film (film_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    store_id integer NOT NULL REFERENCES store (store_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    last_update timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE rental (
    rental_id serial PRIMARY KEY,
    rental_date timestamptz NOT NULL,
    inventory_id integer NOT NULL REFERENCES inventory (inventory_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    customer_id integer NOT NULL REFERENCES customer (customer_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    return_date timestamptz,
    staff_id integer NOT NULL REFERENCES staff (staff_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    last_update timestamptz NOT NULL DEFAULT now(),
    UNIQUE (rental_date, inventory_id, customer_id)
);
CREATE TABLE payment (
    payment_id serial PRIMARY KEY,
    customer_id integer NOT NULL REFERENCES customer (customer_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    staff_id integer NOT NULL REFERENCES staff (staff_id) ON UPDATE CASCADE ON DELETE RESTRICT,
    rental_id integer REFERENCES rental (rental_id) ON UPDATE CASCADE ON DELETE SET NULL,
    group_no bigint NOT NULL DEFAULT nextval('payment_group_seq'),
    amount numeric(5,2) NOT NULL,
    payment_date timestamptz NOT NULL
);
-- Composite key and composite foreign key.
CREATE TABLE rental_item (
    rental_id integer NOT NULL REFERENCES rental (rental_id) ON DELETE CASCADE,
    line_no integer NOT NULL,
    qty integer NOT NULL DEFAULT 1,
    PRIMARY KEY (rental_id, line_no)
);
CREATE TABLE rental_item_note (
    note_id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    rental_id integer NOT NULL,
    line_no integer NOT NULL,
    note text,
    FOREIGN KEY (rental_id, line_no) REFERENCES rental_item (rental_id, line_no) ON DELETE CASCADE
);
CREATE TABLE film_review (
    review_id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    film_id integer NOT NULL REFERENCES film (film_id),
    customer_id integer REFERENCES customer (customer_id),
    stars smallint NOT NULL CHECK (stars BETWEEN 1 AND 5),
    body text
);
CREATE TABLE rental_archive (
    archive_id integer NOT NULL,
    rental_date date NOT NULL,
    customer_id integer
) PARTITION BY RANGE (rental_date);
CREATE TABLE rental_archive_2023 PARTITION OF rental_archive FOR VALUES FROM ('2023-01-01') TO ('2024-01-01');
CREATE TABLE rental_archive_2024 PARTITION OF rental_archive FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');

CREATE SCHEMA erd_aux;
CREATE TABLE erd_aux.audit_log (
    audit_id bigserial PRIMARY KEY,
    customer_id integer REFERENCES public.customer (customer_id),
    note text
);

CREATE INDEX idx_film_title ON film (title);
CREATE INDEX idx_film_fulltext ON film USING gin (fulltext);
CREATE INDEX idx_film_lower_title ON film (lower(title)) WHERE length IS NOT NULL;
CREATE INDEX idx_fk_city_id ON address (city_id);
CREATE UNIQUE INDEX idx_unq_customer_email ON customer (lower(email));
CREATE UNIQUE INDEX idx_unq_actor_name ON actor (first_name, last_name);
CREATE INDEX idx_rental_customer ON rental (customer_id);

CREATE TRIGGER film_last_updated BEFORE UPDATE ON film FOR EACH ROW EXECUTE FUNCTION last_updated();
CREATE TRIGGER actor_last_updated BEFORE UPDATE ON actor FOR EACH ROW EXECUTE FUNCTION last_updated();

CREATE VIEW film_list AS
    SELECT f.film_id, f.title, c.name AS category
    FROM film f
    JOIN film_category fc ON fc.film_id = f.film_id
    JOIN category c ON c.category_id = fc.category_id;
CREATE VIEW customer_list AS
    SELECT cu.customer_id, cu.first_name || ' ' || cu.last_name AS name, a.address, ci.city
    FROM customer cu JOIN address a ON a.address_id = cu.address_id JOIN city ci ON ci.city_id = a.city_id;
CREATE VIEW film_list_long AS SELECT * FROM film_list WHERE length(title) > 10;
CREATE MATERIALIZED VIEW sales_by_store AS
    SELECT s.store_id, sum(p.amount) AS total_sales
    FROM payment p JOIN staff st ON st.staff_id = p.staff_id JOIN store s ON s.store_id = st.store_id
    GROUP BY s.store_id;
CREATE INDEX idx_sales_by_store ON sales_by_store (store_id);

CREATE FUNCTION film_in_stock(p_film_id integer, p_store_id integer) RETURNS SETOF integer
LANGUAGE sql STABLE AS $$
    SELECT inventory_id FROM inventory WHERE film_id = p_film_id AND store_id = p_store_id
$$;
CREATE PROCEDURE touch_actor(p_id integer) LANGUAGE sql AS $$
    UPDATE actor SET first_name = first_name WHERE actor_id = p_id
$$;

COMMENT ON SCHEMA erd_aux IS 'Auxiliary schema';
COMMENT ON TABLE film IS 'Films; one row per title';
COMMENT ON COLUMN film.rating IS 'MPAA rating';
COMMENT ON COLUMN customer.email IS E'Contact e-mail (it''s optional)';
COMMENT ON VIEW film_list IS 'Films with their category';
COMMENT ON FUNCTION film_in_stock(integer, integer) IS 'Inventory ids of a film in a store';
COMMENT ON TYPE mpaa_rating IS 'MPAA film ratings';
COMMENT ON SEQUENCE payment_group_seq IS 'Payment grouping';

INSERT INTO country (country) VALUES ('Vietnam'), ('Japan'), ('Peru');
INSERT INTO language (name) VALUES ('English'), ('Vietnamese');
INSERT INTO category (name, parent_category_id) VALUES ('Action', NULL), ('Drama', 1);
INSERT INTO film (title, language_id) VALUES ('Alpha', 1), ('Omega', 2);
INSERT INTO film_category VALUES (1, 1), (2, 2);
INSERT INTO film_review (film_id, stars) VALUES (1, 5);
INSERT INTO erd_aux.audit_log (note) VALUES ('first');
SELECT setval('payment_group_seq', 115);
