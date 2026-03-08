#!/usr/bin/env bash
# Run fixture tests every 60s, push stats to AGENT-STATE.md on change.
set -euo pipefail
cd "$(dirname "$0")"

REPO_DIR="/home/claude-code/development/rust-react-compiler"
LAST_CORRECT=""
LAST_COMPILE=""

log() { echo "[$(date '+%H:%M:%S')] $*"; }

run_tests() {
  cd "$REPO_DIR/rust-react-compiler"
  cargo test --test fixtures run_all_fixtures -- --ignored 2>&1 \
    | grep -E "Correct rate|Compile rate" | tail -5
}

update_and_push() {
  local compile_line="$1" correct_line="$2"
  local compile_pct correct_pct compile_frac correct_frac

  compile_pct=$(echo "$compile_line" | grep -oE '[0-9]+\.[0-9]+' | head -1)
  compile_frac=$(echo "$compile_line" | grep -oE '\([0-9]+/[0-9]+\)' | head -1)
  correct_pct=$(echo "$correct_line" | grep -oE '[0-9]+\.[0-9]+' | head -1)
  correct_frac=$(echo "$correct_line" | grep -oE '\([0-9]+/[0-9]+\)' | head -1)

  [[ -z "$correct_pct" || -z "$compile_pct" ]] && { log "Could not parse metrics"; return 1; }

  cd "$REPO_DIR"

  # Update metrics table
  sed -i "s/| Compile rate |.*/| Compile rate | ${compile_pct}% ${compile_frac} all fixtures |/" AGENT-STATE.md
  sed -i "s/| Correct rate |.*/| Correct rate | ${correct_pct}% ${correct_frac} all fixtures |/" AGENT-STATE.md

  log "Metrics: compile=${compile_pct}% ${compile_frac}, correct=${correct_pct}% ${correct_frac}"

  git add AGENT-STATE.md
  if git diff --cached --quiet; then
    log "No AGENT-STATE.md changes to commit"
    return 0
  fi

  git commit -m "$(printf 'chore: stats update — %s correct, %s compile\n\nCo-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>' "${correct_pct}% ${correct_frac}" "${compile_pct}% ${compile_frac}")"
  git push origin main
  log "Pushed stats to remote"
}

log "stats-loop started — running tests every 60s"

while true; do
  log "Running fixture tests..."
  METRICS=$(run_tests 2>/dev/null || true)

  COMPILE=$(echo "$METRICS" | grep "Compile rate" | tail -1)
  CORRECT=$(echo "$METRICS" | grep "Correct rate" | tail -1)

  CORRECT_FRAC=$(echo "$CORRECT" | grep -oE '\([0-9]+/[0-9]+\)' | head -1)
  COMPILE_FRAC=$(echo "$COMPILE" | grep -oE '\([0-9]+/[0-9]+\)' | head -1)

  if [[ "$CORRECT_FRAC" != "$LAST_CORRECT" || "$COMPILE_FRAC" != "$LAST_COMPILE" ]]; then
    log "Stats changed: compile=${COMPILE_FRAC} correct=${CORRECT_FRAC}"
    update_and_push "$COMPILE" "$CORRECT" || true
    LAST_CORRECT="$CORRECT_FRAC"
    LAST_COMPILE="$COMPILE_FRAC"
  else
    log "No change: correct=${CORRECT_FRAC} compile=${COMPILE_FRAC}"
  fi

  sleep 60
done
