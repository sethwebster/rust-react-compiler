# Agent State

**AGENTS: Read this file first. Update it throughout your session.**

---

## Protocol

### Session Start (required)
1. Read this file completely
2. Run `git log --oneline -10` and `git diff HEAD --stat` to verify current state
3. Run `cargo test --test fixtures run_all_fixtures -- --ignored 2>&1 | grep -E "Correct rate|Compile rate|Error"` to get baseline metrics
4. Review `## Todo List` — claim your first item, mark it `→ in progress`
5. Begin work

### During Session (required)
- Cross off items as you complete them: `- [ ]` → `- [x]`
- Add newly discovered tasks to `## Todo List`
- Update `## Current Task` when you switch focus

### Session End (required)
Update the following before stopping:
- **Metrics** — current compile rate, correct rate, error counts
- **Current Task** — what the next agent should start on
- **Completed This Session** — concrete list of files changed and what changed
- **Todo List** — cross off completed items, add new tasks
- **Blocked On** — current blockers
- **Key Invariants** — anything you had to re-derive
- **History** — append one row to the History table

---

## Metrics (as of last update)

| Metric | Value |
|--------|-------|
| Compile rate | 84.2% (1048/1244) |
| Correct rate | 24.4% (304/1244) |
| Error (expected) | 196 |
| Error (unexpected) | 0 |
| Uncommitted changes | none |

---

## Current Task

**Active plan**: [`plans/correctness-300-to-500.md`](./plans/correctness-300-to-500.md)

Investigating highest-impact improvements to correctness. Attempted full SCCP constant propagation (branch pruning, phi eval, unary folding) but it regressed 304→299 — reverted. Now analyzing 744 mismatch fixtures to find the most impactful category to fix.

---

## Todo List

> This list is displayed live at https://rust-react-compiler.sethwebster.workers.dev
> Format: `- [ ] pending` / `- [x] done`. Maintain throughout your session, not just at the end.

### Phase 1: Codegen Quick Wins (target: 284 → 310-320)
- [x] Fix `$tN` internal temp leak in codegen — name_hint resolution (+1)
- [x] For-loop init reassembly in codegen (+1, update blocked by DCE)
- [x] Lambda hoisting to `_temp` form — pipeline reorder + DCE protection (+1)

### Phase 2: Codegen Naming + Control Flow (target: → 350)
- [ ] Use original identifier in memo blocks instead of `tN` alias (94 files, 22 exclusive)
- [ ] Switch codegen cleanup — remove `bb0:` labels (11 files, 7 exclusive)
- [ ] Try/catch codegen (6 files, 3 exclusive)

### Phase 3: Scope Analysis (target: → 400+)
- [ ] Scope merging — merge scopes that invalidate on same deps
- [ ] Reactive dep propagation through while/for loops (sentinel overuse)
- [ ] Cache slot count correction (downstream of scope fixes)

### Ongoing / Deferred
- [ ] Fix destructured parameter lowering (`lower/core.rs`, `lower/functions.rs`)
- [x] Define `ReactiveFunction` / `ReactiveScope` types in `hir.rs`
- [ ] Implement `build_reactive_function` — wire into `pipeline.rs` after scope inference
- [ ] Fix `codegen_reactive_function` stub to operate on `ReactiveFunction`
- [ ] Fix `prune_non_reactive_dependencies` (PARTIAL → REAL)
- [ ] Fix `constant_propagation` (PARTIAL → REAL)
- [x] Fix compile regression: thread `phi_operands` through dep-resolution callsites
- [x] Port `build_reactive_scope_terminals_hir` (guarded by `RC_ENABLE_SCOPE_TERMINALS_HIR`)
- [x] Port `flatten_reactive_loops_hir` (guarded by `RC_ENABLE_FLATTEN_REACTIVE_LOOPS`)

---

## Completed This Session

- Previous sessions: see History table below
- Attempted full SCCP constant_propagation rewrite (branch pruning, phi eval, unary/binary folding, graph cleanup) — regressed 304→299, reverted
- Analyzing 744 mismatch fixtures to identify highest-impact fix category
---

## Blocked On

- `build_reactive_function` is PARTIAL (initial skeleton only) — still **critical path blocker**
  - Blocks: full `codegen_reactive_function`, `rename_variables`, and downstream scope transforms
  - Needs: scope terminals + full terminal/branch/loop coverage in tree builder
- Codegen (`hir_codegen.rs`) currently operates on raw `HIR`, not `ReactiveFunction`
  - Fix requires full tree build + dual codegen integration first
- Enabling `RC_ENABLE_SCOPE_TERMINALS_HIR=1` currently regresses correctness (24.1% → 20.2%)
  - Known symptom: some loops emit incorrect iterator source (`for...of undefined`) and duplicate returns (`return x; return x;`)
  - Example fixture: `for-of-simple.js` (correct baseline, regresses under scope-terminals flag)

---

## Pass Status Map

| Pass | File | Status | LOC |
|------|------|--------|-----|
| enter_ssa | ssa/enter_ssa.rs | REAL | 826 |
| eliminate_redundant_phi | ssa/eliminate_redundant_phi.rs | REAL | 344 |
| rewrite_instruction_kinds | ssa/rewrite_instruction_kinds... | REAL | ~50 |
| infer_mutation_aliasing_ranges | inference/infer_mutation_aliasing_ranges.rs | REAL | 390 |
| infer_reactive_places | inference/infer_reactive_places.rs | REAL | 331 |
| aliasing_effects | inference/aliasing_effects.rs | REAL | 98 |
| analyse_functions | inference/analyse_functions.rs | STUB | 5 |
| drop_manual_memoization | inference/drop_manual_memoization.rs | REAL | 126 |
| inline_iife | inference/inline_iife.rs | DEFERRED | 7 |
| infer_mutation_aliasing_effects | inference/infer_mutation_aliasing_effects.rs | STUB | 7 |
| dead_code_elimination | optimization/dead_code_elimination.rs | REAL | 331 |
| outline_functions | optimization/outline_functions.rs | REAL | 353 |
| constant_propagation | optimization/constant_propagation.rs | PARTIAL | ~37 |
| optimize_props_method_calls | optimization/optimize_props_method_calls.rs | STUB | 2 |
| optimize_for_ssr | optimization/optimize_for_ssr.rs | STUB | 2 |
| outline_jsx | optimization/outline_jsx.rs | STUB | 2 |
| prune_maybe_throws | optimization/prune_maybe_throws.rs | STUB | 2 |
| infer_reactive_scope_variables | reactive_scopes/infer_reactive_scope_variables.rs | REAL | 540 |
| merge_reactive_scopes_that_invalidate_together | reactive_scopes/merge_reactive_scopes... | REAL | 441 |
| propagate_scope_dependencies_hir | reactive_scopes/propagate_scope_dependencies_hir.rs | REAL | 274 |
| merge_overlapping_reactive_scopes_hir | reactive_scopes/merge_overlapping... | REAL | 125 |
| prune_unused_scopes | reactive_scopes/prune_unused_scopes.rs | REAL | 180 |
| promote_used_temporaries | reactive_scopes/promote_used_temporaries.rs | REAL | 45 |
| prune_non_reactive_dependencies | reactive_scopes/prune_non_reactive_dependencies.rs | PARTIAL | 15 |
| **build_reactive_function** | reactive_scopes/build_reactive_function.rs | **PARTIAL** | **~500** |
| build_reactive_scope_terminals_hir | reactive_scopes/build_reactive_scope_terminals_hir.rs | PARTIAL (flagged) | ~320 |
| codegen_reactive_function | reactive_scopes/codegen_reactive_function.rs | STUB | 14 |
| align_method_call_scopes | reactive_scopes/align_method_call_scopes.rs | STUB | 2 |
| align_object_method_scopes | reactive_scopes/align_object_method_scopes.rs | STUB | 2 |
| align_reactive_scopes_to_block_scopes_hir | reactive_scopes/align_reactive_scopes... | REAL | ~305 |
| assert_well_formed_break_targets | reactive_scopes/assert_well_formed_break_targets.rs | STUB | 2 |
| extract_scope_declarations_from_destructuring | reactive_scopes/extract_scope_decl... | STUB | 2 |
| flatten_reactive_loops_hir | reactive_scopes/flatten_reactive_loops_hir.rs | PARTIAL (flagged) | ~50 |
| flatten_scopes_with_hooks_or_use_hir | reactive_scopes/flatten_scopes... | STUB | 2 |
| memoize_fbt_and_macro_operands | reactive_scopes/memoize_fbt_and_macro_operands.rs | STUB | 2 |
| propagate_early_returns | reactive_scopes/propagate_early_returns.rs | STUB | 2 |
| prune_always_invalidating_scopes | reactive_scopes/prune_always_invalidating_scopes.rs | STUB | 2 |
| prune_hoisted_contexts | reactive_scopes/prune_hoisted_contexts.rs | STUB | 2 |
| prune_non_escaping_scopes | reactive_scopes/prune_non_escaping_scopes.rs | REAL | 282 |
| prune_unused_labels | reactive_scopes/prune_unused_labels.rs | STUB | 2 |
| prune_unused_labels_hir | reactive_scopes/prune_unused_labels_hir.rs | STUB | 2 |
| prune_unused_lvalues | reactive_scopes/prune_unused_lvalues.rs | STUB | 2 |
| rename_variables | reactive_scopes/rename_variables.rs | STUB | 2 |
| stabilize_block_ids | reactive_scopes/stabilize_block_ids.rs | STUB | 2 |
| validate_hooks_usage | validation/validate_hooks_usage.rs | PARTIAL | 28 |
| validate_no_ref_access_in_render | validation/validate_no_ref_access_in_render.rs | PARTIAL | 11 |
| validate_exhaustive_dependencies | validation/validate_exhaustive_dependencies.rs | STUB | 3 |
| validate_no_capitalized_calls | validation/validate_no_capitalized_calls.rs | STUB | 3 |
| validate_no_derived_computations_in_effects | validation/validate_no_derived... | STUB | 3 |
| validate_no_freezing_known_mutable_functions | validation/validate_no_freezing... | STUB | 3 |
| validate_no_jsx_in_try_statement | validation/validate_no_jsx_in_try_statement.rs | STUB | 3 |
| validate_no_set_state_in_effects | validation/validate_no_set_state_in_effects.rs | STUB | 3 |
| validate_preserved_manual_memoization | validation/validate_preserved... | STUB | 3 |
| validate_source_locations | validation/validate_source_locations.rs | STUB | 3 |
| validate_static_components | validation/validate_static_components.rs | STUB | 3 |
| name_anonymous_functions | transform/name_anonymous_functions.rs | STUB | 3 |

---

## Key Invariants (don't re-derive)

- **Identifiers**: stored by `IdentifierId` (u32 newtype). Use `env.identifier(id)` to look up.
- **Blocks**: stored in `IndexMap<BlockId, BasicBlock>` in **reverse-postorder**.
- **Place**: stores `IdentifierId`, not a pointer. Identifier data lives in `Environment.identifiers`.
- **No lifetimes on HIR types** — all owned `String`s.
- **oxc 0.69** — AST types differ from Babel. Don't assume Babel node shapes.
- **`ReactiveFunction` types are defined in `hir.rs`** — keep tree/codegen logic aligned with existing variants.
- **Codegen operates on HIR directly** — intentional temporary architectural mismatch.
- **serde** on all HIR types — requires `indexmap = { features = ["serde"] }`.
- **TS source**: `react/compiler/packages/babel-plugin-react-compiler/src/`
- **Fixtures**: `react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler/`

---

## Architecture

```
oxc parse → pre-lowering validators → HIR lowering → SSA → inference →
optimization → reactive scope inference → reactive scope transforms →
build_reactive_function ← CRITICAL MISSING PIECE →
codegen (currently bypasses ReactiveFunction) → oxc_codegen → JS output
```

---

## History

| Date | Compile % | Correct % | Overall % | Passes Real | Stubs |
|------|-----------|-----------|-----------|-------------|-------|
| 2026-03-02 | 84.2 | 17.3 | 29 | 14 | 38 |
| 2026-03-02 | 84.2 | 21.5 | — | 14 | 38 | codegen, SSA, scope passes |
| 2026-03-02 | 84.2 | 22.0 | — | 15 | 37 | drop_manual_memoization, IIFE unwrap |
| 2026-03-03 | 84.2 | 22.7 | — | 16 | 36 | PruneNonEscapingScopes (DeclarationId), dep hoisting |
| 2026-03-03 | 84.2 | 22.8 | — | 16 | 36 | optional chaining fix, mismatch analysis, plan |
| 2026-03-03 | 84.2 | 23.1 | — | 16 | 36 | Phase 1 codegen: $tN leak, for-init, lambda hoisting |
| 2026-03-03 | 84.2 | 23.8 | — | 16 | 36 | switch braces (+3), for-loop update DCE + continue (+6) |
| 2026-03-03 | 84.2 | 23.8 | — | 16 | 36 | ralph-loop iter1: flatten_reactive_loops deferred, near-miss analysis |
| 2026-03-03 | 84.2 | 24.1 | — | 16 | 36 | ralph-loop iter2: alloc dep tracing (+4), rename_variables deferred |
| 2026-03-03 | 84.2 | 24.1 | — | 16 | 36 | ralph-loop iter3: tree builder skeleton, scope inference investigation |
| 2026-03-03 | 84.2 | 24.1 | — | 16 | 36 | fixed propagate_scope_dependencies compile regression (`phi_operands` threading) |
| 2026-03-03 | 84.2 | 24.1 | — | 16 | 36 | implemented scope-terminals + loop-flatten passes behind flags; scoped regression isolated (`for-of`/duplicate return under flag) |
| 2026-03-04 | 84.2 | 24.4 | — | 17 | 35 | align_reactive_scopes_to_block_scopes_hir: stub→REAL (+4 correct, 300→304) |
