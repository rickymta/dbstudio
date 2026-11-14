#!/usr/bin/env bash
# Turns the failures in a cargo log into GitHub annotations (readable without downloading logs):
# one multi-line annotation per compiler error block and per failing test's captured output.
# GitHub keeps 10 error annotations per step, so at most 10 are emitted.
# Usage: annotate-failures.sh <log> [title]
log=$1
title=${2:-failure}
[ -f "$log" ] || exit 0
awk '
  function flush() { if (b != "") print b; b = ""; n = 0; rustc = 0 }
  /^---- .* stdout ----$/ { flush(); b = $0; n = 1; next }
  /^error(\[E[0-9]+\])?: / { flush(); b = $0; n = 1; rustc = 1; next }
  b != "" && rustc && /^$/ { flush(); next }
  b != "" && (/^failures:$/ || /^test result:/ || /^warning: build failed/) { flush(); next }
  b != "" && n < 40 { b = b "\x01" $0; n++ }
  END { flush() }
' "$log" | head -10 | while IFS= read -r block; do
  msg=${block//%/%25}
  msg=${msg//$'\r'/}
  msg=${msg//$'\x01'/%0A}
  echo "::error title=$title::$msg"
done
grep -E "^test .* FAILED$" "$log" | head -40 || true
