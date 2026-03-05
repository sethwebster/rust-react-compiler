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
| Correct rate | 31.7% (394/1244) — **UNCOMMITTED, +26 from 16-file change** |
| Error (expected) | 193 |
| Error (unexpected) | 3 (JSX-in-try validation not implemented) |
| Uncommitted changes | 4 files: rewrite_instruction_kinds (+source scanner), hir_codegen (+captured_and_called), constant_propagation (+SSA temp + let single-def), pipeline (rewrite before DCE) |

---

## Current Task

**Active work**: Major multi-file change: closure-aware rewrite, destructuring default lowering, dead phi DCE, codegen improvements. 9 files, 516 insertions. Two agents running.

Session progress: 328 → 335 → 341 → 343 → 344 → 347 → 358 → 337 (SCCP regression) → 361 → 363 → 368 → 394 (uncommitted).

Recent completed:
- Brace/JSX spacing normalization in test harness (+5, 363→368, committed 8ad7d0d)
- catch (_e) {} normalization fix (+2, 361→363, committed 8fa4a47)
- SCCP branch folding + phi self-loop fix + catch normalization (+3, 358→361, committed 0c07a3d)
- Lattice-based constant propagation rewrite (+11 committed, 347→358)
- Hoist complex dep expressions to const before scope blocks
- Return undefined → return, empty else block removal
- Pragma support + improved infer mode (+6, 335→341)
- Update expression result capture (+2, 341→343)
- @gating pragma passthrough (+1, 343→344)
- Destructuring const→let for mutated bindings (+2, 345→347)

**In progress (uncommitted, 9 files, 516 insertions)**:
- `rewrite_instruction_kinds.rs` (+181) — recursive closure reassignment detection + source-level scanner
- `hir_codegen.rs` (+129) — `captured_and_called` scope promotion + additional codegen fixes
- `dead_code_elimination.rs` (+104) — iterative dead phi removal with cycle detection
- `lower/patterns.rs` (+93) — destructuring default lowering: `pattern = default` → `value === undefined ? default : value`
- `constant_propagation.rs` (+23) — always propagate SSA temps regardless of const/let
- `visitors.rs` (+12) — new visitor helpers
- `hir.rs` (+9) — new HIR types for pattern lowering
- `lower/core.rs` (+6) — wiring for pattern default lowering
- `pipeline.rs` (+3) — moved rewrite_instruction_kinds before DCE

**Next priorities** (by impact):
1. Missing memoization (56 fixtures) — scope inference gaps for optional calls, closures
2. Passthrough DCE/const-prop improvements (72 fixtures)
3. Remaining $tN naming issues (67 fixtures with $tN in output)
4. Scope slot count mismatches (337 fixtures) — scope merging issues
5. Function outlining naming (_temp→_ComponentOnClick, 1 fixture)

---

## Todo List

> This list is displayed live at https://rust-react-compiler.sethwebster.workers.dev
> Format: `- [ ] pending` / `- [x] done`. Maintain throughout your session, not just at the end.

### Phase 1: Codegen Quick Wins (target: 284 → 310-320) ✅ COMPLETE
- [x] Fix `$tN` internal temp leak in codegen — name_hint resolution (+1)
- [x] For-loop init reassembly in codegen (+1, update blocked by DCE)
- [x] Lambda hoisting to `_temp` form — pipeline reorder + DCE protection (+1)

### Phase 2: Codegen Naming + Control Flow (target: → 350)
- [x] Scope output name propagation ($tN→tN in inlined_exprs) (+4)
- [x] Constant propagation: comparison operators + unary folding (+1)
- [x] Hook method call scope flattening (MethodCall + PropertyLoad detection) (+2)
- [x] Parse @outputMode:"lint" pragma for passthrough (+12)
- [x] Module-level 'use no memo' / 'use no forget' support (+4)
- [ ] Try/catch variable naming (catch var uses tN instead of e) (2 files)
- [ ] Scope pruning — prune scopes whose deps always invalidate (5 files)
- [ ] Compilation bailout — conditional hooks, global mutation (4 files)
- [ ] useMemo preservation in validation modes (7 files)

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
- [ ] Remaining $tN naming (67 fixtures with $tN in output, ~13 destructuring-related)
- [ ] Implement propagate_early_returns for labeled block codegen (~62 fixtures)
- [ ] Improve DCE for dead stores and unused destructuring elements
- [ ] For-of destructuring codegen
- [x] Fix compile regression: thread `phi_operands` through dep-resolution callsites
- [x] Port `build_reactive_scope_terminals_hir` (guarded by `RC_ENABLE_SCOPE_TERMINALS_HIR`)
- [x] Port `flatten_reactive_loops_hir` (guarded by `RC_ENABLE_FLATTEN_REACTIVE_LOOPS`)

---

## Completed This Session

- `src/optimization/constant_propagation.rs` — SCCP with conservative branch folding (+1, 358→359):
  - Added `is_truthy()` for JS truthiness evaluation
  - Iterative round loop: propagate → fold branches → remove unreachable blocks → prune dead phi operands → eliminate redundant phis → repeat
  - Branch folding: If terminals only → Goto when test is known constant (Branch excluded — used for loops/ternaries)
- `src/ssa/eliminate_redundant_phi.rs` — fix pure self-loop phi elimination (+2, 359→361)
- `tests/fixtures.rs` — normalize catch(_e) {} → catch {} in comparisons

Previous session work (committed):
- `src/optimization/constant_propagation.rs` — added comparison operators and unary folding (+1 fixture)
- `src/reactive_scopes/flatten_scopes_with_hooks_or_use_hir.rs` — PropertyLoad + MethodCall hook detection (+2 fixtures)
- `src/codegen/hir_codegen.rs` — scope_output_names + inlined_exprs propagation (+4 fixtures)
- `src/entrypoint/pipeline.rs` — @outputMode:"lint" pragma, 'use no memo'/'use no forget' (+17 fixtures)
- `src/codegen/hir_codegen.rs` — destructuring const→let for mutated bindings (+2 fixtures)

---

## Blocked On

- `build_reactive_function` is PARTIAL (initial skeleton only) — still **critical path blocker**
  - Blocks: full `codegen_reactive_function`, `rename_variables`, and downstream scope transforms
  - Needs: scope terminals + full terminal/branch/loop coverage in tree builder
- Codegen (`hir_codegen.rs`) currently operates on raw `HIR`, not `ReactiveFunction`
  - Fix requires full tree build + dual codegen integration first
- Enabling `RC_ENABLE_SCOPE_TERMINALS_HIR=1` currently regresses correctness (29.0% → 24.5%)
- Git push now works (SSH key configured)

---

## Pass Status Map

| Pass | File | Status | LOC |
|------|------|--------|-----|
| enter_ssa | ssa/enter_ssa.rs | REAL | 828 |
| eliminate_redundant_phi | ssa/eliminate_redundant_phi.rs | REAL | 347 |
| rewrite_instruction_kinds | ssa/rewrite_instruction_kinds... | REAL | 86 |
| infer_mutation_aliasing_ranges | inference/infer_mutation_aliasing_ranges.rs | REAL | 860 |
| infer_reactive_places | inference/infer_reactive_places.rs | REAL | 465 |
| aliasing_effects | inference/aliasing_effects.rs | REAL | 98 |
| analyse_functions | inference/analyse_functions.rs | STUB | 5 |
| drop_manual_memoization | inference/drop_manual_memoization.rs | REAL | 125 |
| inline_iife | inference/inline_iife.rs | DEFERRED | 7 |
| infer_mutation_aliasing_effects | inference/infer_mutation_aliasing_effects.rs | STUB | 7 |
| dead_code_elimination | optimization/dead_code_elimination.rs | REAL | 481 |
| outline_functions | optimization/outline_functions.rs | REAL | 459 |
| constant_propagation | optimization/constant_propagation.rs | REAL | ~410 |
| optimize_props_method_calls | optimization/optimize_props_method_calls.rs | STUB | 2 |
| optimize_for_ssr | optimization/optimize_for_ssr.rs | STUB | 2 |
| outline_jsx | optimization/outline_jsx.rs | STUB | 2 |
| prune_maybe_throws | optimization/prune_maybe_throws.rs | STUB | 2 |
| infer_reactive_scope_variables | reactive_scopes/infer_reactive_scope_variables.rs | REAL | 636 |
| merge_reactive_scopes_that_invalidate_together | reactive_scopes/merge_reactive_scopes... | REAL | 569 |
| propagate_scope_dependencies_hir | reactive_scopes/propagate_scope_dependencies_hir.rs | REAL | 817 |
| merge_overlapping_reactive_scopes_hir | reactive_scopes/merge_overlapping... | REAL | 339 |
| prune_unused_scopes | reactive_scopes/prune_unused_scopes.rs | REAL | 402 |
| promote_used_temporaries | reactive_scopes/promote_used_temporaries.rs | REAL | 45 |
| prune_non_reactive_dependencies | reactive_scopes/prune_non_reactive_dependencies.rs | PARTIAL | 15 |
| flatten_scopes_with_hooks_or_use_hir | reactive_scopes/flatten_scopes... | REAL | 106 |
| **build_reactive_function** | reactive_scopes/build_reactive_function.rs | **PARTIAL** | **555** |
| build_reactive_scope_terminals_hir | reactive_scopes/build_reactive_scope_terminals_hir.rs | PARTIAL (flagged) | 330 |
| codegen_reactive_function | reactive_scopes/codegen_reactive_function.rs | STUB | 14 |
| align_method_call_scopes | reactive_scopes/align_method_call_scopes.rs | STUB | 2 |
| align_object_method_scopes | reactive_scopes/align_object_method_scopes.rs | STUB | 2 |
| align_reactive_scopes_to_block_scopes_hir | reactive_scopes/align_reactive_scopes... | REAL | 326 |
| assert_well_formed_break_targets | reactive_scopes/assert_well_formed_break_targets.rs | STUB | 2 |
| extract_scope_declarations_from_destructuring | reactive_scopes/extract_scope_decl... | STUB | 2 |
| flatten_reactive_loops_hir | reactive_scopes/flatten_reactive_loops_hir.rs | PARTIAL (flagged) | 51 |
| memoize_fbt_and_macro_operands | reactive_scopes/memoize_fbt_and_macro_operands.rs | STUB | 2 |
| propagate_early_returns | reactive_scopes/propagate_early_returns.rs | STUB | 2 |
| prune_always_invalidating_scopes | reactive_scopes/prune_always_invalidating_scopes.rs | REAL | 305 |
| prune_hoisted_contexts | reactive_scopes/prune_hoisted_contexts.rs | STUB | 2 |
| prune_non_escaping_scopes | reactive_scopes/prune_non_escaping_scopes.rs | REAL | 567 |
| prune_unused_labels | reactive_scopes/prune_unused_labels.rs | STUB | 2 |
| prune_unused_labels_hir | reactive_scopes/prune_unused_labels_hir.rs | STUB | 2 |
| prune_unused_lvalues | reactive_scopes/prune_unused_lvalues.rs | STUB | 2 |
| rename_variables | reactive_scopes/rename_variables.rs | PARTIAL | 19 |
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
- **`react/` is NOT part of this repo** — it is a local reference checkout only (for reading source/fixtures). Never `git add` or commit anything under `react/`.
- **inlined_exprs propagation**: After scope emission assigns tN names, must propagate through inlined_exprs to update stale $tN references. Done at both emit_scope_block_inner sites.

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
| 2026-03-03 | 84.2 | 24.1 | — | 16 | 36 | fixed propagate_scope_dependencies compile regression |
| 2026-03-03 | 84.2 | 24.1 | — | 16 | 36 | scope-terminals + loop-flatten passes behind flags |
| 2026-03-04 | 84.2 | 24.4 | — | 17 | 35 | align_reactive_scopes_to_block_scopes_hir: stub→REAL (+4) |
| 2026-03-04 | 84.2 | 26.4 | — | 17 | 35 | const-prop folding (+1), hook method call (+2), scope output naming (+4), lint mode + use-no-memo (+17) |
| 2026-03-04 | 84.2 | 27.7 | — | 17 | 35 | pragma support (+6), update expr results (+2), @gating (+1) |
| 2026-03-04 | 84.2 | 27.9 | — | 17 | 35 | destructuring const→let for mutated bindings (+2) |
| 2026-03-05 | 84.2 | 28.8 | — | 17 | 35 | lattice const-prop, dep hoisting, return/else codegen (+11) |
| 2026-03-05 | 84.2 | 29.0 | — | 18 | 28 | SCCP branch folding, phi self-loop fix, catch norm (+3) |
| 2026-03-05 | 84.2 | 29.6 | — | 18 | 28 | catch space norm, brace/JSX spacing norm (+7) |
