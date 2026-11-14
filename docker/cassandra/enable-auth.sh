#!/bin/sh
# Entrypoint wrapper of the Cassandra test containers: switches the authenticator to
# PasswordAuthenticator (default login cassandra/cassandra) and starts Cassandra as usual. Handles
# both the 4.x one-line and the 5.x block form of the `authenticator` setting.
set -eu
yaml=/etc/cassandra/cassandra.yaml
awk '
  /^authenticator:/ { print "authenticator: PasswordAuthenticator"; skip = 1; next }
  /^authorizer:/ { print "authorizer: CassandraAuthorizer"; skip = 1; next }
  skip && /^[[:space:]]/ { next }
  { skip = 0; print }
' "$yaml" > /tmp/cassandra.yaml
cat /tmp/cassandra.yaml > "$yaml"
# Materialized views and user-defined functions are off by default (names differ between 4.x and
# 5.x: set both spellings, unknown keys are ignored by 4.1 only when absent, so append only the
# one the file already knows).
for key in materialized_views_enabled enable_materialized_views user_defined_functions_enabled enable_user_defined_functions; do
  if grep -q "^#\?[[:space:]]*$key:" "$yaml"; then
    sed -i "s/^#\?[[:space:]]*$key:.*/$key: true/" "$yaml"
  fi
done
exec docker-entrypoint.sh cassandra -f
