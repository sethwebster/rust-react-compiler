#!/usr/bin/env bash
# Monitor agent progress — update AGENT-STATE.md and push to GitHub on new commits.

set -euo pipefail
cd "$(dirname "$0")"

POLL_INTERVAL=30
METRICS_INTERVAL=120
LAST_COMMIT=""
LAST_METRICS_TIME=0

log() { echo "[$(date '+%H:%M:%S')] $*"; }

run_metrics() {
  log "Running fixture tests..."
  cargo test --test fixtures run_all_fixtures -- --ignored 2>&1 \
    | grep -E "Correct rate|Compile rate|correct:|compile:" | tail -20
}

get_rate() {
  local label="$1" output="$2"
  echo "$output" | grep -oE "${label}: [0-9.]+% \([0-9]+/[0-9]+\)" | tail -1
}

push_state_update() {
  local msg="$1"
  git add AGENT-STATE.md
  if git diff --cached --quiet; then
    log "No AGENT-STATE.md changes to commit"
    return 0
  fi
  git commit -m "$(printf 'chore: update AGENT-STATE.md — %s\n\nCo-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>' "$msg")"
  git push origin main
  log "Pushed — website updating"
}

update_metrics_in_file() {
  local compile_line="$1" correct_line="$2"
  local compile_pct correct_pct compile_frac correct_frac

  compile_pct=$(echo "$compile_line" | grep -oE '[0-9]+\.[0-9]+' | head -1)
  compile_frac=$(echo "$compile_line" | grep -oE '\([0-9]+/[0-9]+\)' | head -1)
  correct_pct=$(echo "$correct_line" | grep -oE '[0-9]+\.[0-9]+' | head -1)
  correct_frac=$(echo "$correct_line" | grep -oE '\([0-9]+/[0-9]+\)' | head -1)

  [[ -z "$correct_pct" || -z "$compile_pct" ]] && return 1

  sed -i "s/| Compile rate |.*/| Compile rate | ${compile_pct}% ${compile_frac} |/" AGENT-STATE.md
  sed -i "s/| Correct rate |.*/| Correct rate | ${correct_pct}% ${correct_frac} |/" AGENT-STATE.md

  log "Updated metrics: compile=${compile_pct}% ${compile_frac}, correct=${correct_pct}% ${correct_frac}"
  return 0
}

log "Monitor started. Poll interval: ${POLL_INTERVAL}s, metrics interval: ${METRICS_INTERVAL}s"
LAST_COMMIT=$(git rev-parse HEAD)

while true; do
  sleep "$POLL_INTERVAL"

  git fetch --quiet origin main 2>/dev/null || true
  REMOTE_HEAD=$(git rev-parse origin/main 2>/dev/null || echo "")
  LOCAL_HEAD=$(git rev-parse HEAD)
  NEW_HEAD="${REMOTE_HEAD:-$LOCAL_HEAD}"

  if [[ "$NEW_HEAD" == "$LAST_COMMIT" ]]; then
    continue
  fi

  NEW_COMMITS=$(git log --oneline "${LAST_COMMIT}..${NEW_HEAD}" 2>/dev/null | head -10)
  [[ -z "$NEW_COMMITS" ]] && { LAST_COMMIT="$NEW_HEAD"; continue; }

  log "New commits:"
  echo "$NEW_COMMITS"

  # Pull if remote is ahead
  if [[ "$REMOTE_HEAD" != "$LOCAL_HEAD" ]]; then
    git pull --ff-only origin main 2>/dev/null || true
  fi
  LAST_COMMIT=$(git rev-parse HEAD)

  # Skip if only AGENT-STATE updates
  MEANINGFUL=$(echo "$NEW_COMMITS" | grep -v "chore: update AGENT-STATE" || true)
  if [[ -z "$MEANINGFUL" ]]; then
    log "Only housekeeping commits, skipping"
    continue
  fi

  FIRST=$(echo "$MEANINGFUL" | head -1)
  SHORT_MSG=$(echo "$FIRST" | cut -c9-70)

  NOW=$(date +%s)
  if (( NOW - LAST_METRICS_TIME >= METRICS_INTERVAL )); then
    METRICS=$(run_metrics)
    COMPILE=$(get_rate "Compile rate" "$METRICS")
    CORRECT=$(get_rate "Correct rate" "$METRICS")
    LAST_METRICS_TIME=$NOW

    if update_metrics_in_file "$COMPILE" "$CORRECT"; then
      CORRECT_SHORT=$(echo "$CORRECT" | grep -oE '[0-9.]+% \([0-9]+/[0-9]+\)')
      push_state_update "${CORRECT_SHORT} — ${SHORT_MSG}"
    fi
  else
    SECS_SINCE=$(( NOW - LAST_METRICS_TIME ))
    log "Metrics ran ${SECS_SINCE}s ago — updating current task only"
    COMMIT_SHA=$(echo "$FIRST" | cut -c1-7)
    # Update the "In progress" line with latest commit
    sed -i "s/\*\*In progress (uncommitted)\*\*:.*/\*\*In progress (uncommitted)\*\*: +${COMMIT_SHA} ${SHORT_MSG}/" AGENT-STATE.md
    push_state_update "+${COMMIT_SHA} ${SHORT_MSG}"
  fi
done
