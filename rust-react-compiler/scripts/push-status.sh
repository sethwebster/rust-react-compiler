#!/usr/bin/env bash
# Usage: push-status.sh <type> <message> [compile_rate] [correct_rate]
# Types: status, progress, milestone
#
# Env: PUSH_URL (default: https://isreactcompilerrustyet.com/api/push)
#      PUSH_SECRET (required)

set -euo pipefail

TYPE="${1:?Usage: push-status.sh <type> <message> [compile_rate] [correct_rate]}"
MSG="${2:?Usage: push-status.sh <type> <message> [compile_rate] [correct_rate]}"
COMPILE_RATE="${3:-}"
CORRECT_RATE="${4:-}"

URL="${PUSH_URL:-https://isreactcompilerrustyet.com/api/push}"
SECRET="${PUSH_SECRET:?PUSH_SECRET must be set}"

# Escape double quotes in message
MSG_ESCAPED="${MSG//\"/\\\"}"

# Build JSON body
if [[ -n "$COMPILE_RATE" || -n "$CORRECT_RATE" ]]; then
  METRICS='"metrics":{'
  [[ -n "$COMPILE_RATE" ]] && METRICS+="\"compileRate\":$COMPILE_RATE"
  [[ -n "$COMPILE_RATE" && -n "$CORRECT_RATE" ]] && METRICS+=","
  [[ -n "$CORRECT_RATE" ]] && METRICS+="\"correctRate\":$CORRECT_RATE"
  METRICS+='}'
  BODY="{\"type\":\"$TYPE\",\"message\":\"$MSG_ESCAPED\",$METRICS}"
else
  BODY="{\"type\":\"$TYPE\",\"message\":\"$MSG_ESCAPED\"}"
fi

curl -sS -X POST "$URL" \
  -H "Authorization: Bearer $SECRET" \
  -H "Content-Type: application/json" \
  -d "$BODY"
