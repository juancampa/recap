#!/bin/sh
# Print one line for each new or changed entry, as the server finds them.
# The script reads the change events, and asks the server for the entries
# that changed after the last version that it saw.
set -eu
RECAP=${RECAP:-http://127.0.0.1:7717}
version=$(curl -sf "$RECAP/v1/status" | jq .version)
curl -sfN "$RECAP/v1/events?format=ndjson" |
  jq --unbuffered -r 'select(.type == "changed") | .version' |
  while read -r next; do
    curl -sfG "$RECAP/v1/history" --data-urlencode "since=$version" |
      jq -r '.groups[].entries[] | "\(.timestamp[11:19])  \(.source)  \(if .kind.type == "prompt" then "> " + (.kind.text | gsub("\\s+"; " ") | .[0:80]) else .file_path end)"'
    version=$next
  done
