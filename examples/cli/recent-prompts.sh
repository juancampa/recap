#!/bin/sh
# Print the newest prompts of a project, one line for each prompt.
# Usage: recent-prompts.sh [<project root>] [<count>]
set -eu
RECAP=${RECAP:-http://127.0.0.1:7717}
project=${1:-$(pwd)}
count=${2:-10}
curl -sfG "$RECAP/v1/history" \
  --data-urlencode "project=$project" \
  --data-urlencode "kind=prompts" \
  --data-urlencode "limit=$count" |
  jq -r '.groups[].entries[] | "\(.id)  \(.timestamp[0:16])  \(.kind.text | gsub("\\s+"; " ") | .[0:100])"'
