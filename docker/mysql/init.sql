-- Runs once when a MySQL / MariaDB test container initialises its data directory.
-- The integration tests create scratch databases named dbstudio_it_* as the `dbstudio` user;
-- grant that up front so no test depends on another one having run first.
GRANT ALL PRIVILEGES ON `dbstudio\_it%`.* TO 'dbstudio'@'%';
FLUSH PRIVILEGES;
