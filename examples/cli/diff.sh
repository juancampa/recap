#!/bin/sh
# Print the diff of one entry as unified text.
# Usage: diff.sh <entry id>
set -eu
RECAP=${RECAP:-http://127.0.0.1:7717}
curl -sfG "$RECAP/v1/diff" --data-urlencode "ids=$1" --data-urlencode "format=unified"
