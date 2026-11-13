#!/usr/bin/env bash
# Run the Oracle driver tests inside the oracle-it image against the running `oraclefree` service.
#
# Unlike `docker compose run oracle-it` (which mounts the directory that holds compose.test.yml),
# this mounts the repository you run it from, so it also works from a git worktree.
#
#   docker/oracle-it/run-tests.sh                 # cargo test -p dbstudio-driver-oracle
#   docker/oracle-it/run-tests.sh -- --nocapture  # extra arguments go to cargo test
#
# Needs: the compose project `dbstudio-test` network with `oraclefree` up and healthy,
# and the image `dbstudio-test-oracle-it` (docker compose -f docker/compose.test.yml build oracle-it).
# Cargo output and the registry live in named volumes so the slow amd64 build is done once.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
network="${DBSTUDIO_IT_NETWORK:-dbstudio-test_default}"
image="${DBSTUDIO_IT_ORACLE_IMAGE:-dbstudio-test-oracle-it:latest}"
suffix="$(basename "$root" | tr -c 'a-zA-Z0-9' '-')"

exec docker run --rm --platform linux/amd64 \
  --network "$network" \
  --memory "${DBSTUDIO_IT_ORACLE_IT_MEM:-3g}" \
  -v "$root":/work \
  -v "dbstudio-oracle-it-target-$suffix":/target-oracle \
  -v "dbstudio-oracle-it-registry":/root/.cargo/registry \
  -e DBSTUDIO_IT="${DBSTUDIO_IT:-1}" \
  -e DBSTUDIO_IT_ORACLE_HOST="${DBSTUDIO_IT_ORACLE_HOST:-oraclefree}" \
  -e DBSTUDIO_IT_ORACLE_PORT="${DBSTUDIO_IT_ORACLE_PORT_IN_NETWORK:-1521}" \
  -e DBSTUDIO_IT_ORACLE_SERVICE="${DBSTUDIO_IT_ORACLE_SERVICE:-FREEPDB1}" \
  -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" \
  -w /work \
  "$image" \
  cargo test -p dbstudio-driver-oracle "$@"
