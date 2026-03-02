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
- **Current Task** — what's next (for the next agent)
- **Completed This Session** — concrete list of what you finished this session
- **Todo List** — cross off completed items, add newly discovered tasks
- **Blocked On** — current blockers
- **Key Invariants** — anything you had to re-derive
- **History** — append one row to the History table

---

## Metrics (as of last update)

| Metric | Value |
|--------|-------|
| Compile rate | 84.2% (1048/1244) |
| Correct rate | 17.3% |
| Error (expected) | 196 |
| Error (unexpected) | 0 |
| Uncommitted changes | 25 files, +4386/-265 lines |

---

## Current Task

**Fixing destructured parameter lowering** (in progress by another agent)

Relevant files:
- `rust-react-compiler/src/hir/lower/core.rs`
- `rust-react-compiler/src/hir/lower/expressions.rs`
- `rust-react-compiler/src/hir/lower/functions.rs`

---

## Todo List

> This list is displayed live at https://rust-react-compiler.sethwebster.workers.dev
> Format: `- [ ] pending` / `- [x] done`. Maintain throughout your session, not just at the end.

- [ ] Fix destructured parameter lowering (`lower/core.rs`, `lower/functions.rs`)
- [ ] Define `ReactiveFunction` / `ReactiveScope` types in `hir.rs` (model after `ReactiveFunction.ts`, `ReactiveScope.ts`)
- [ ] Implement `build_reactive_function` — input: `&HIRFunction`, output: `ReactiveFunction`; wire into `pipeline.rs`
- [ ] Fix `codegen_reactive_function` stub to operate on `ReactiveFunction` (unlocks correct rate improvement)
- [ ] Fix `prune_non_reactive_dependencies` (PARTIAL → REAL)
- [ ] Fix `constant_propagation` (PARTIAL → REAL)

---

## Completed This Session

- `prune_unused_scopes.rs`: expanded from stub to real implementation
- `codegen/hir_codegen.rs`: major expansion (1,816 → 2,902 LOC)
- `pipeline.rs`: wired additional passes (433 → 677 LOC)
- `enter_ssa.rs`: extended (+93 lines)
- `infer_reactive_scope_variables.rs`: expanded (467 → 540 LOC)
- `propagate_scope_dependencies_hir.rs`: expanded (174 → 274 LOC)
- `merge_overlapping_reactive_scopes_hir.rs`: expanded (103 → 125 LOC)
- `prune_non_reactive_dependencies.rs`: expanded (2 → 15 LOC)
- Introduced **Correct rate** metric to fixture harness (was only tracking compile rate)

---

## Blocked On

- `build_reactive_function` is still a 2-LOC stub — **critical path blocker**
  - Blocks: `codegen_reactive_function`, `rename_variables`, and all downstream scope passes
  - Needs: `ReactiveFunction` type defined in `hir.rs` first
- Codegen (`hir_codegen.rs`) currently operates on raw `HIR`, not `ReactiveFunction`
  - This is an architectural mismatch that limits correct rate ceiling
  - Fix requires `build_reactive_function` to exist first

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
| drop_manual_memoization | inference/drop_manual_memoization.rs | STUB | 5 |
| inline_iife | inference/inline_iife.rs | STUB | 5 |
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
| **build_reactive_function** | reactive_scopes/build_reactive_function.rs | **STUB** | **2** |
| build_reactive_scope_terminals_hir | reactive_scopes/build_reactive_scope_terminals_hir.rs | STUB | 2 |
| codegen_reactive_function | reactive_scopes/codegen_reactive_function.rs | STUB | 14 |
| align_method_call_scopes | reactive_scopes/align_method_call_scopes.rs | STUB | 2 |
| align_object_method_scopes | reactive_scopes/align_object_method_scopes.rs | STUB | 2 |
| align_reactive_scopes_to_block_scopes_hir | reactive_scopes/align_reactive_scopes... | STUB | 2 |
| assert_well_formed_break_targets | reactive_scopes/assert_well_formed_break_targets.rs | STUB | 2 |
| extract_scope_declarations_from_destructuring | reactive_scopes/extract_scope_decl... | STUB | 2 |
| flatten_reactive_loops_hir | reactive_scopes/flatten_reactive_loops_hir.rs | STUB | 2 |
| flatten_scopes_with_hooks_or_use_hir | reactive_scopes/flatten_scopes... | STUB | 2 |
| memoize_fbt_and_macro_operands | reactive_scopes/memoize_fbt_and_macro_operands.rs | STUB | 2 |
| propagate_early_returns | reactive_scopes/propagate_early_returns.rs | STUB | 2 |
| prune_always_invalidating_scopes | reactive_scopes/prune_always_invalidating_scopes.rs | STUB | 2 |
| prune_hoisted_contexts | reactive_scopes/prune_hoisted_contexts.rs | STUB | 2 |
| prune_non_escaping_scopes | reactive_scopes/prune_non_escaping_scopes.rs | STUB | 2 |
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

- **Identifiers**: stored by `IdentifierId` (u32 newtype), not by reference. Use `env.identifier(id)` to look up.
- **Blocks**: stored in `IndexMap<BlockId, BasicBlock>` in **reverse-postorder**. Iteration order = RPO.
- **Place**: stores `IdentifierId`, not a pointer. Identifier data lives in `Environment.identifiers`.
- **No lifetimes on HIR types** — all owned `String`s. Avoids borrow complexity at cost of cloning.
- **oxc 0.69** for parsing — AST types differ from Babel. Don't assume Babel node shapes.
- **`ReactiveFunction` type does NOT exist yet** — do not reference it until `hir.rs` is updated.
- **Codegen operates on HIR directly** — architectural mismatch vs TS compiler (which uses ReactiveFunction). Intentional temporary state.
- **serde** on all HIR types — requires `indexmap = { features = ["serde"] }` in Cargo.toml.
- **TS source location**: `react/compiler/packages/babel-plugin-react-compiler/src/`
- **Fixture dir**: `react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler/`

---

## Architecture

```
oxc parse
    ↓
pre-lowering validators (core.rs)
    ↓
HIR lowering (lower/)
    ↓
SSA construction (ssa/)
    ↓
inference passes (inference/)
    ↓
optimization passes (optimization/)
    ↓
reactive scope inference (reactive_scopes/infer_*)
    ↓
reactive scope transforms (reactive_scopes/ — mostly stubs)
    ↓
build_reactive_function  ← CRITICAL MISSING PIECE
    ↓
codegen (codegen/hir_codegen.rs — currently bypasses ReactiveFunction)
    ↓
oxc_codegen → JS output
```

---

## History

Agents: append one row here at the end of every session with actual measured values.

| Date | Compile % | Correct % | Overall % | Passes Real | Stubs |
|------|-----------|-----------|-----------|-------------|-------|
| 2026-03-02 | 84.2 | 17.3 | 29 | 14 | 38 |
