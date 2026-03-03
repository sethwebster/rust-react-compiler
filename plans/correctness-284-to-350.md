# Plan: Correctness 284 → 350+ (22.8% → 28%+)

**Created**: 2026-03-03
**Baseline**: 284/1244 correct (22.8%), 1048/1244 compile (84.2%), 0 unexpected errors

## Problem

764 fixtures compile but produce wrong output. Analysis of 500 mismatches reveals 7 distinct root causes, most in codegen (`hir_codegen.rs`).

## Mismatch Categories

| # | Category | Affected | Exclusive | Effort | Target |
|---|----------|----------|-----------|--------|--------|
| 1 | `$tN` internal temps leaked into output | 46 | 13 | Easy | Phase 1 |
| 2 | For-loop init/update lost in codegen | 26 | 14 | Medium | Phase 1 |
| 3 | Lambda not hoisted to `_temp` | 41 | 14 | Easy | Phase 1 |
| 4 | `tN` alias instead of original identifier | 94 | 22 | Easy | Phase 2 |
| 5 | Switch codegen (`bb0:` label, case braces) | 11 | 7 | Easy | Phase 2 |
| 6 | Try/catch missing from output | 6 | 3 | Easy | Phase 2 |
| 7 | Wrong scope contents / boundary placement | 158 | 130 | Hard | Phase 3 |
| 8 | Sentinel overuse (`=== sentinel` vs `!== dep`) | 121 | 1 | Hard | Phase 3 |
| 9 | Cache slot count wrong (too few/many) | 273 | 60 | Medium | Phase 3 |

## Phase 1: Codegen Quick Wins (~41 exclusive fixtures)

### 1a. Fix `$tN` temp leak (46 files, 13 exclusive)

**Problem**: HIR internal temporaries (`$t38`, `$t18`) appear in output instead of being resolved.

**File**: `src/codegen/hir_codegen.rs`

**Fix**: In codegen, when emitting a variable name that starts with `$t`, resolve it:
- If the temp is assigned from a single source (LoadLocal/StoreLocal), use the source name
- If the temp is a computation result, generate a `tN` name (without `$` prefix)

### 1b. For-loop init/update reassembly (26 files, 14 exclusive)

**Problem**: For-loop init extracted before loop, update expression lost entirely. Output: `const x = 0; for (; cond; )` instead of `for (let x = 0; cond; x++)`.

**File**: `src/codegen/hir_codegen.rs` (for-loop terminal codegen)

**Fix**: In `ForTerminal` codegen:
- Detect the init instruction(s) that were extracted before the loop
- Re-assemble into `for (init; test; update)` form
- Preserve update expression from the HIR terminal

### 1c. Lambda hoisting to `_temp` (41 files, 14 exclusive)

**Problem**: Arrow functions passed to hooks/callbacks stay inline instead of being extracted to `function _temp() {}` at top level.

**File**: `src/codegen/hir_codegen.rs` + `src/optimization/outline_functions.rs`

**Fix**: Check `outline_functions.rs` — it's REAL (353 LOC) but may not be triggering for all cases. Likely a codegen issue where outlined functions aren't being emitted in `_temp` form.

## Phase 2: Codegen Naming + Control Flow (~32 exclusive fixtures)

### 2a. Use original identifier in memo blocks (94 files, 22 exclusive)

**Problem**: Codegen emits `let t0; if (...) { t0 = []; } const arr = t0;` instead of `let arr; if (...) { arr = []; }`.

**File**: `src/codegen/hir_codegen.rs`

**Fix**: When a scope's output is immediately assigned to a named variable via StoreLocal, use that variable name directly in the memo block instead of a fresh `tN` temp.

### 2b. Switch codegen (11 files, 7 exclusive)

**Problem**: Switch cases emit `bb0:` block labels, wrong brace form.

**File**: `src/codegen/hir_codegen.rs`

**Fix**: Clean up switch terminal codegen to emit standard `case X: { ... }` without HIR block labels.

### 2c. Try/catch codegen (6 files, 3 exclusive)

**Problem**: Try/catch blocks missing from output entirely.

**File**: `src/codegen/hir_codegen.rs`

**Fix**: Implement try/catch terminal codegen (currently likely a stub or fallthrough).

## Phase 3: Scope Analysis (130+ exclusive fixtures) — Future

### 3a. Scope merging

Scopes that invalidate on the same deps should merge. `merge_reactive_scopes_that_invalidate_together.rs` exists (441 LOC, REAL) but misses cases.

### 3b. Reactive dep propagation through loops

While/for loop control variables aren't tracked as reactive deps, causing sentinel overuse.

### 3c. Cache slot counting

Downstream of scope boundary correctness — will improve automatically as 3a/3b improve.

## Verification

After each phase:
```bash
cargo test --test fixtures run_all_fixtures -- --ignored --nocapture 2>&1 | grep 'Output correct:'
```

Phase 1 target: 284 → ~310-320 correct
Phase 2 target: ~320 → ~350 correct
Phase 3 target: ~350 → 400+ correct

## Unresolved Questions

- Is `outline_functions.rs` actually running and producing `_temp` fns, or is codegen ignoring the outline results?
- For-loop: does the HIR `ForTerminal` carry the init/update info, or is it lost during lowering?
- `$tN` leak: is this a naming pass issue (`rename_variables` is STUB) or codegen?
