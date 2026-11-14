# PostgreSQL 16 with the PL/pgSQL debugger (pldebugger / `pldbgapi`) for the SQL debugger tests.
# The official postgres image already has the PGDG apt repository configured.
FROM postgres:16
RUN apt-get update \
 && apt-get install -y --no-install-recommends postgresql-16-pldebugger \
 && rm -rf /var/lib/apt/lists/*
# Runs once, in the `dbstudio` database, when the data directory is created.
COPY init/ /docker-entrypoint-initdb.d/
