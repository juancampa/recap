#!/bin/sh
# Search the diffs and the prompts with a case-insensitive regex, and print
# the 20 newest matches.
# Usage: search.sh <regex> [<project root>]
set -eu
RECAP=${RECAP:-http://127.0.0.1:7717}
if [ $# -ge 2 ]; then
  set -- "$1" --data-urlencode "project=$2"
else
  set -- "$1"
fi
query=$1
shift
curl -sfG "$RECAP/v1/search/content" --data-urlencode "q=$query" "$@" |
  jq -r '.matches[0:20][].id' |
  while read -r id; do
    curl -sf "$RECAP/v1/entries/$id" |
      jq -r '"\(.id)  \(.timestamp[0:16])  \(if .kind.type == "prompt" then "> " + (.kind.text | gsub("\\s+"; " ") | .[0:80]) else .file_path end)"'
  done
