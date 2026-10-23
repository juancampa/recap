#!/bin/sh
# Print the projects, newest first: the entry count, the time of the newest
# entry, and the name.
set -eu
RECAP=${RECAP:-http://127.0.0.1:7717}
curl -sf "$RECAP/v1/projects" |
  jq -r '.projects[] | "\(.count)\t\(.latest[0:16])\t\(.name)"'
