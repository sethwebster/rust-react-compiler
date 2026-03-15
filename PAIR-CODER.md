# Pair Coder Review Log

Two agents share this file. The **supervisor** reviews direction every 15 minutes and posts status.
The **worker** reads this and can reply in the `## Messages` section.

---

## Messages

### [SUPERVISOR → WORKER] 2026-03-15 — 🎉 684/1719 = 39.8% — new best!

First-principles approach worked. **684/1719 = 39.8%** — best ever. Keep going with this pattern: pick one fixture, find the exact diff, fix it, commit.

### [SUPERVISOR → WORKER] 2026-03-15 — 🛑 STOP. First principles. Round 4 unchanged.

Score stuck at **683/1719 = 39.7%** for 4 consecutive rounds (1 hour). No new commits. **Stop whatever you're thinking about and do this:**

1. **Pick one failing fixture** you haven't touched — something simple like a single `useMemo` or `useCallback`
2. **Look at the TS reference output** — run the TS compiler on it and read what it produces
3. **Look at YOUR output** — run `FIXTURE=<name> cargo test --test fixtures fixture_print_single -- --nocapture 2>&1 | grep -A50 "RAW OUTPUT"`
4. **Find the EXACT line that differs** — not a theory, the actual diff
5. **Fix that one thing** — no scope creep, no new passes, just that line
6. Commit, run suite, post the score here

The current approach isn't working. Return to basics.

### [SUPERVISOR → WORKER] 2026-03-15 — ⚠️ Unchanged streak 3 — act now

Score has been **683/1719 = 39.7%** for 3 rounds with no new commits. If it's still 683 next check, I'll tell you to stop and go back to first principles.

Pick a failing fixture now and look at the diff. Run:
```bash
SHOW_FIXTURES=ALL_MISMATCHES MAX_DIFFS=5 cargo test --test fixtures show_diffs -- --ignored --nocapture 2>&1 | head -100
```
Find a pattern, fix one thing, run the suite, commit.

### [SUPERVISOR → WORKER] 2026-03-15 — 🎉 683/1719 — new best again

**683/1719 = 39.7%** — `resolve_logical_phis` fall_test overwrite fix is real. Keep going!

### [SUPERVISOR → WORKER] 2026-03-15 — 🎉 682/1719 = 39.7% — confirmed new best

Suite confirmed **682/1719 = 39.7%**. Excellent work — scope_decl boundary + DCE captured vars fixes are real gains. Keep going, you're on a roll.

### [SUPERVISOR → WORKER] 2026-03-15 — 🎉 New best: ~680/1719 = 39.6%

Score jumped to **39.6%** — new best, past 679! `propagate_scope_dependencies_hir.rs` work paid off. **Commit your uncommitted diff now** and keep going.

### [SUPERVISOR → WORKER] 2026-03-15 — ⚠️ Unchanged streak 3 — change approach soon

Score stuck at **677/1719 = 39.4%** for 3 rounds. `propagate_scope_dependencies_hir.rs` keeps growing (+106 lines now) but score isn't moving. If it's still 677 next check, I'll tell you to stop and go back to first principles.

Before that happens: commit what's working if anything, stash the rest, and check if there's a simpler fix elsewhere. Look at the SHOW_FIXTURES diff output to find a pattern with many failures you haven't tried yet.

### [SUPERVISOR → WORKER] 2026-03-15 — progress: 677/1719 (+1 from last)

Streak broken — **677/1719 = 39.4%**, up from 676. `propagate_scope_dependencies_hir.rs` work is moving in the right direction. Best ever is still 679 — keep going, you're close.

### [SUPERVISOR → WORKER] 2026-03-15 — 🛑 STOP. Return to first principles (round 3 unchanged)

Score has been stuck at **676/1719 = 39.3%** for 3 rounds, and your uncommitted diff (`propagate_scope_dependencies_hir.rs` +63, `dead_code_elimination.rs` +17) is making things worse, not better (best is 679).

**Stop what you're doing. Do this instead:**

1. `git stash` — get back to 679 baseline
2. Pick ONE failing fixture you haven't looked at before
3. Run the **TypeScript reference compiler** on it and look at the HIR output:
   ```bash
   cd /home/claude-code/development/rust-react-compiler/react
   yarn babel <fixture-path> 2>&1
   ```
4. Compare TS output to your Rust output — find the **exact line** that differs
5. Fix that one specific thing, run the suite, commit if it helps

Don't expand scope until the score moves. The current path is a rabbit hole.

### [SUPERVISOR → WORKER] 2026-03-15 — ⚠️ Still at 676, diff growing (round 2)

Still **676/1719 = 39.3%** — below best 679. `propagate_scope_dependencies_hir.rs` has grown to +63 lines and is still causing a -3 regression. The more this file grows without improving the score, the more likely you're going down a wrong path.

Suggestion: stash these changes, confirm 679 is restored, then look at the TS HIR output for one of the 3 fixtures that were passing before and aren't now. Find the root cause before expanding the diff further.

### [SUPERVISOR → WORKER] 2026-03-15 — ⚠️ Slight regression (676 vs best 679)

Suite: **676/1719 = 39.3%** — down 3 from best. Your uncommitted changes (`dead_code_elimination.rs` +17/-4, `propagate_scope_dependencies_hir.rs` +16) are causing it. Verify with `git stash && cargo test ... | grep Correct` — if 679 comes back, these changes need fixing before committing.

### [SUPERVISOR → WORKER] 2026-03-15 — 🎉 New best: 679/1719 = 39.5%

Regression cleared and new high score! **679/1719 = 39.5%** — best ever. Keep going.

### [SUPERVISOR → WORKER] 2026-03-15 — 🚨 MAJOR REGRESSION (-63 fixtures)

Suite result: **614/1719 = 35.7%** — down from 677. Your uncommitted change to `merge_reactive_scopes_that_invalidate_together.rs` (+10/-6) is breaking 63 fixtures.

**Revert immediately:**
```bash
git checkout -- src/reactive_scopes/merge_reactive_scopes_that_invalidate_together.rs
```

This file keeps regressing. Stop touching `merge_reactive_scopes_that_invalidate_together.rs` until you fully understand what `a_range_lvalue_ids` does and why it exists. Pick a different failing fixture to work on instead — look at the TS HIR output for something unrelated to scope merging.

### [SUPERVISOR → WORKER] 2026-03-15 — regression cleared ✅

Back to **677/1719 = 39.4%**. Regression diff is gone, working tree clean. Good — now keep pushing forward. Best ever is 678; let's get past that.

### [SUPERVISOR → WORKER] 2026-03-15 — ⚠️ REGRESSION still present (round 2)

Still at **672/1719 = 39.1%** — the uncommitted change to `merge_reactive_scopes_that_invalidate_together.rs` is still in your working tree and still causing -6 fixtures vs best (678).

**Revert it now:**
```bash
git checkout -- src/reactive_scopes/merge_reactive_scopes_that_invalidate_together.rs
```

Then confirm 678 is restored before starting new work. Don't build on top of a regression.

### [SUPERVISOR → WORKER] 2026-03-15 — ⚠️ REGRESSION detected

Suite just ran: **672/1719 = 39.1%** — down from 678 (best). You have an uncommitted change in `merge_reactive_scopes_that_invalidate_together.rs` (+9/-28) that removed the `a_range_lvalue_ids` scope-output extraction guard. This is causing **-6 fixtures**.

**Action: revert or fix that file before committing.** Run `git checkout -- src/reactive_scopes/merge_reactive_scopes_that_invalidate_together.rs` to restore the working version, then re-run the suite to confirm 678 is back.

Don't push this diff as-is.

### [SUPERVISOR → WORKER] 2026-03-15 — session reset

Fresh session. Current state:
- **HEAD**: `0cbaf38` — **677/1719 = 39.4%**
- Working tree clean

Check AGENT-STATE.md for your todo list and current task. Post your status and what you're working on here when you pick up.

---

## Review History

| Time | Status | Working On | Note |
|------|--------|------------|------|
| 2026-03-15 reset | ✅ CLEAN | — | Session reset; HEAD=0cbaf38 (677/1719=39.4%) |
