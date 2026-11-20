#!/bin/sh
# Pull the images of the test databases, retrying when the registry answers with an error
# (rate limit, timeout). `docker compose up` pulls only once and fails the whole CI job.
#
#   sh docker/pull-images.sh [profile] [service...]
#
# An empty profile means the default services. Images built locally (`build:`) are skipped.
# PULL_ATTEMPTS (default 5) and PULL_WAIT_SECS (default 30, multiplied by the attempt number)
# tune the retries.
set -u

profile="${1:-}"
[ "$#" -gt 0 ] && shift

dir="$(dirname "$0")"
attempts="${PULL_ATTEMPTS:-5}"
wait_secs="${PULL_WAIT_SECS:-30}"

pull() {
  if [ -n "$profile" ]; then
    docker compose -f "$dir/compose.test.yml" --profile "$profile" pull --ignore-buildable --quiet "$@"
  else
    docker compose -f "$dir/compose.test.yml" pull --ignore-buildable --quiet "$@"
  fi
}

attempt=1
while :; do
  if pull "$@"; then
    exit 0
  fi
  if [ "$attempt" -ge "$attempts" ]; then
    echo "pull-images: still failing after $attempts attempts" >&2
    exit 1
  fi
  delay=$((attempt * wait_secs))
  echo "pull-images: attempt $attempt failed, retrying in ${delay}s" >&2
  sleep "$delay"
  attempt=$((attempt + 1))
done
