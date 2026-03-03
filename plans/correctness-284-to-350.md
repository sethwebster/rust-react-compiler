# Plan: Correctness 287 → 350+ (23.1% → 28%+)

**Created**: 2026-03-03
**Revised**: 2026-03-03 (after Phase 1 execution)
**Baseline**: 287/1244 correct (23.1%), 1048/1244 compile (84.2%), 0 unexpected errors

## What We Learned in Phase 1

The initial mismatch categorization overestimated "exclusive" counts. Many issues are entangled:

- **`$tN` leak** (projected 13 exclusive → actual +1): Most leaks need `rename_variables` pass, not just codegen fixes. The `ident_name` name_hint fix only helped outlined functions.
- **For-loop** (projected 14 exclusive → actual +1): Init reassembly works, but DCE removes update block instructions. Needs loop-aware DCE.
- **Lambda hoisting** (projected 14 exclusive → actual +1): Was a pipeline ordering bug (DCE before outline_functions). Fixed, but only 1 fixture was exclusively blocked by this.
- **Named identifier in memo blocks** (projected 22 exclusive → investigated, deferred): Requires coordinated `is_named_var` + emission changes. Simple approaches regress 50+ fixtures.

**Lesson**: "Exclusive" fixture counts from mismatch analysis are upper bounds. Many fixtures have multiple overlapping issues; fixing one exposes the next.

## Revised Categories

| # | Category | Affected | Realistic gain | Effort | Phase |
|---|----------|----------|---------------|--------|-------|
| 1 | `rename_variables` (stub → real) | 94+ | **+10-20** | Medium | **2** |
| 2 | Switch codegen | 11 | **+5-7** | Easy | **2** |
| 3 | Try/catch codegen | 6 | **+2-3** | Easy | **2** |
| 4 | For-loop update (DCE fix) | 26 | **+3-5** | Medium | **2** |
| 5 | Named identifier in memo blocks | 94 | **+5-10** | Hard | 3 |
| 6 | Scope merging improvements | 158 | **+20-40** | Hard | 3 |
| 7 | Reactive dep propagation through loops | 121 | **+10-20** | Hard | 3 |
| 8 | Cache slot count correction | 273 | cascading | — | 3 |

## Phase 1: COMPLETE (284 → 287, +3)

- [x] `ident_name` FunctionExpression name_hint resolution
- [x] For-loop init reassembly into `for(...)` header
- [x] Pipeline: `outline_functions` before DCE
- [x] DCE: protect outlined FunctionExpressions
- [x] Codegen: skip outlined FunctionExpression as stmt
- [x] Self-assignment guards in all 3 scope emission paths

## Phase 2: Pass-Level Fixes (target: 287 → 305-315)

### 2a. `rename_variables` — STUB → REAL (~+10-20)

**The single highest-impact item.** The TS compiler's `renameVariables` assigns sequential `t0`, `t1`, etc. to promoted temporaries. Our stub is 2 LOC. Without it:
- `$tN` temps leak into output (46 files)
- Named outputs use wrong temp names (94 files)
- Scope outputs get extra `const arr = t0;` aliases

**File**: `src/reactive_scopes/rename_variables.rs` (currently `pub fn run(_hir: &mut HIRFunction) {}`)

**What it needs to do**:
1. Walk all instructions
2. For each identifier that is a promoted temporary (no user-facing name, starts with `$t`), assign a sequential name `t0`, `t1`, etc.
3. Write the name into `env.identifiers[id].name`

This unlocks gains across multiple categories simultaneously.

### 2b. Switch codegen (~+5-7)

**Problem**: Switch cases emit `bb0:` block labels, wrong brace form.

**File**: `src/codegen/hir_codegen.rs`

**Fix**: Clean up switch terminal codegen to emit standard `case X: { ... }` without HIR block labels.

### 2c. Try/catch codegen (~+2-3)

**Problem**: Try/catch blocks missing from output entirely.

**File**: `src/codegen/hir_codegen.rs`

**Fix**: Implement try/catch terminal codegen.

### 2d. For-loop update — DCE fix (~+3-5)

**Problem**: DCE removes for-loop update block instructions because the loop variable's SSA phi isn't marked as "used".

**File**: `src/optimization/dead_code_elimination.rs`

**Fix**: In DCE, when processing `Terminal::For { update: Some(bid), .. }`, mark all identifiers in the update block as used (they're needed for loop semantics even if the loop variable doesn't escape).

## Phase 3: Scope Analysis (target: 305 → 350+) — Future

### 3a. Named identifier in memo blocks
Requires coordinated changes to `analyze_scope` is_named_var logic + scope block emission. Can't use simple `name.is_some()` — needs to distinguish const scope outputs from const internal declarations.

### 3b. Scope merging
`merge_reactive_scopes_that_invalidate_together.rs` (441 LOC, REAL) misses cases where scopes share deps.

### 3c. Reactive dep propagation through loops
While/for loop control variables → sentinel overuse.

### 3d. Cache slot counting
Downstream of scope correctness — auto-improves as 3b/3c improve.

## Verification

```bash
cargo test --test fixtures run_all_fixtures -- --ignored --nocapture 2>&1 | grep 'Output correct:'
```

## Unresolved Questions

- `rename_variables`: how does the TS compiler decide which temps to rename? Is it all unnamed identifiers, or only those that are "promoted" by `promote_used_temporaries`?
- For-loop DCE: should we mark all update-block idents as used, or add `Terminal::For` to `collect_terminal_uses`?
- Named identifier: can we detect "this is the scope's final const output" reliably without the `is_named_var` regression?
