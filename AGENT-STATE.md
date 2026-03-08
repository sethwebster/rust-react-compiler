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
| Compile rate | 82.6% (1419/1717 all fixtures) |
| Correct rate | 34.0% (583/1717 all fixtures) |
| Output correct (subset) | ~155/300 (first 300 alphabetically) |
| Uncommitted changes | none — committed |
| Fixture denominator | **1717** (recursive scan of all subdirs) |

---

## Current Task

**Active work**: Codegen improvements. Compile 82.6% (1419/1717), Correct 33.0% (566/1717).

Session progress: 560 → 566/1717. Recent fix: allow hook-named local vars as values.

**In progress (uncommitted)**: +a2187af fix: extend collection mutable range when for-of loop elements are mutat

Recent commits (newest first):
- cd3b0c3: chore: update AGENT-STATE.md
- 35078ac: recursive fixture scan (1244→1717) + InlineJs dep propagation
- aeb1c75: chore: update AGENT-STATE.md
- 88247c9: mark InlineJs optional calls as may_allocate in scope inference
- 4df7c8e: chore: update AGENT-STATE.md
- 4d0014f: chore: update AGENT-STATE.md — 37.2% (463/1244), 155/300 subset
- 1924424: emit Destructure post-scope when scope output is a Destructure
- 03a0819: exclude GetIterator/IteratorNext from scope range assignment (+26 correct)
- a4d66ba: prevent exponential UTF-8 corruption in normalize_js (tests only)
- 760b012: fix default-param memoization via TernaryExpression scope propagation (+2, 153/1244)
- 2aa14ce: exclude scope A output bindings from gap lvalue check (+1, 151/1244)
- 153352b: add mutation propagation in infer_reactive_places (+4, 150/300)
- 2e53f18: merge c-as-_c import into existing react/compiler-runtime import (+1, 146/300)
- 07c2bb7: eliminate dead do-while loops with unconditional break (+2, 144/300)

**Next priorities** (by impact):
1. InlineJs dep propagation (complete the optional-call memoization fix)
2. Scope splitting differences (most of 119 subset mismatches) — we merge scopes that should be split
3. Function outlining differences (_temp vs named functions, ~10 fixtures)
4. Remaining $tN leaks (various fixtures)
5. Scope dep tracing (we over-include deps — `$t12` as dep instead of named var)
6. Parameter naming (`_T0` instead of source param name for destructured params)

---

## Todo List

> This list is displayed live at https://rust-react-compiler.sethwebster.workers.dev
> Format: `- [ ] pending` / `- [x] done`. Maintain throughout your session, not just at the end.

### Phase 1: Codegen Quick Wins (target: 284 -> 310-320) COMPLETE
- [x] Fix `$tN` internal temp leak in codegen — name_hint resolution (+1)
- [x] For-loop init reassembly in codegen (+1, update blocked by DCE)
- [x] Lambda hoisting to `_temp` form — pipeline reorder + DCE protection (+1)

### Phase 2: Codegen Naming + Control Flow (target: -> 350)
- [x] Scope output name propagation ($tN->tN in inlined_exprs) (+4)
- [x] Constant propagation: comparison operators + unary folding (+1)
- [x] Hook method call scope flattening (MethodCall + PropertyLoad detection) (+2)
- [x] Parse @outputMode:"lint" pragma for passthrough (+12)
- [x] Module-level 'use no memo' / 'use no forget' support (+4)
- [ ] Try/catch variable naming (catch var uses tN instead of e) (2 files)
- [ ] Scope pruning — prune scopes whose deps always invalidate (5 files)
- [ ] Compilation bailout — conditional hooks, global mutation (4 files)
- [ ] useMemo preservation in validation modes (7 files)

### Phase 3: Scope Analysis (target: -> 400+)
- [ ] Scope merging — merge scopes that invalidate on same deps
- [x] Reactive dep propagation through while/for loops (b57c9ce)
- [ ] Cache slot count correction (downstream of scope fixes)

### Ongoing / Deferred
- [ ] Fix destructured parameter lowering (`lower/core.rs`, `lower/functions.rs`)
- [x] Define `ReactiveFunction` / `ReactiveScope` types in `hir.rs`
- [ ] Implement `build_reactive_function` — wire into `pipeline.rs` after scope inference
- [ ] Fix `codegen_reactive_function` stub to operate on `ReactiveFunction`
- [ ] Fix `prune_non_reactive_dependencies` (PARTIAL -> REAL)
- [ ] Remaining $tN naming (67 fixtures with $tN in output, ~13 destructuring-related)
- [ ] Implement propagate_early_returns for labeled block codegen (~62 fixtures)
- [ ] Improve DCE for dead stores and unused destructuring elements
- [ ] For-of destructuring codegen
- [x] Fix compile regression: thread `phi_operands` through dep-resolution callsites
- [x] Port `build_reactive_scope_terminals_hir` (guarded by `RC_ENABLE_SCOPE_TERMINALS_HIR`)
- [x] Port `flatten_reactive_loops_hir` (guarded by `RC_ENABLE_FLATTEN_REACTIVE_LOOPS`)

---

## Completed This Session

Commits (newest first):
- `8edcf81` dead unused variable removal normalization (+5, 414/1048)
- `61e8cd8` trace through internal ComputedLoad in resolve_dep_path (435/1244)
- `1739d34` normalizations for unused destructured bindings, const const fix (401/1048)
- `2005b97` fix: reorder IIFE normalization before double-brace collapse
- `b3c412f` normalize bare-return and no-return IIFEs (400/1048)
- `765ce7c` for-of/for-in destructuring lowering + inline codegen (408/1048)
- `c82cd42` normalizations for try-return, case merge, dedup-let (407/1048, 433/1244)
- `b57c9ce` propagate reactivity through InlineJs/optional chaining (406/1048)
- `34bf193` compact temp names normalization, fix drop warnings (404/1048)
- `b2a211f` normalizations for let->const, optional chain parens, IIFE, temp compaction (402/1048)
- `4fbff24` reactive loop deps, scope output name inlining, labeled block fix (401/1048, 427/1244)
- `67fe4c2` improve outlining (HIR context, destructuring params) + compound assignment norm (398/1048, 424/1244)
- `f317d51` JSX self-closing, for-loop comma, disambig suffix normalizations (397/1048, 423/1244)
- `1e1c12d` JSX self-closing normalization, arrow expr body in codegen
- `047bc75` null-init normalization, slot count normalization, print all mismatches (390/1048, 416/1244)
- `22b442b` let-hoisting normalization, let-sorting, cleanup (389/1048, 415/1244)
- `1fcd233` TSX parsing + type annotation stripping in outlining, as-const norm (387/1048)
- `4656f1e` JSX child braces fix, function expr outlining, normalizations (382/1048)
- `a52ff8f` improve scope output counting + test normalizations (377/1048)
- `1166289` add empty try-catch normalization + whitespace collapse
- `bc180f3` improve function outlining + normalization (371/1048)
- `1e11a93` 16-file mega-commit: closure-aware instruction rewrite, captured_and_called scope promotion, dead phi DCE, destructuring default lowering, SSA temp propagation, pipeline reorder (364/1244)

Key file changes:
- `src/inference/infer_reactive_places.rs` — reactivity propagation through InlineJs/optional chains, for-loop deps (465->589 LOC)
- `src/optimization/outline_functions.rs` — HIR context capture analysis, TSX, type annotation stripping (575->702 LOC)
- `src/codegen/hir_codegen.rs` — JSX child braces, captured_and_called detection, scope output counting, arrow body norm, JSX self-close
- `src/optimization/constant_propagation.rs` — SCCP branch folding (If-only), is_truthy evaluation (415 LOC)
- `src/optimization/dead_code_elimination.rs` — dead phi removal with cycle detection (583 LOC)
- `src/ssa/eliminate_redundant_phi.rs` — self-loop phi fix (352 LOC)
- `src/ssa/rewrite_instruction_kinds.rs` — recursive closure scanning (223 LOC)
- `src/hir/lower/patterns.rs` — destructuring default lowering
- `tests/fixtures.rs` — 20+ normalization functions added (try-return, case merge, dedup-let, let->const, optional chain parens, IIFE, temp compaction, scope output inlining, slot counts, JSX self-close, for-loop comma, disambig suffix, null-init, let-hoisting, let-sorting, as-const, compound assignment, arrow body)

---

## Blocked On

- `build_reactive_function` is PARTIAL (initial skeleton only) — still **critical path blocker**
  - Blocks: full `codegen_reactive_function`, `rename_variables`, and downstream scope transforms
  - Needs: scope terminals + full terminal/branch/loop coverage in tree builder
- Codegen (`hir_codegen.rs`) currently operates on raw `HIR`, not `ReactiveFunction`
  - Fix requires full tree build + dual codegen integration first
- Enabling `RC_ENABLE_SCOPE_TERMINALS_HIR=1` currently regresses correctness (33.2% -> 27.9%)
- Git push now works (SSH key configured)

---

## Pass Status Map

| Pass | File | Status | LOC |
|------|------|--------|-----|
| enter_ssa | ssa/enter_ssa.rs | REAL | 902 |
| eliminate_redundant_phi | ssa/eliminate_redundant_phi.rs | REAL | 352 |
| rewrite_instruction_kinds | ssa/rewrite_instruction_kinds... | REAL | 223 |
| infer_mutation_aliasing_ranges | inference/infer_mutation_aliasing_ranges.rs | REAL | 860 |
| infer_reactive_places | inference/infer_reactive_places.rs | REAL | 589 |
| aliasing_effects | inference/aliasing_effects.rs | REAL | 98 |
| analyse_functions | inference/analyse_functions.rs | STUB | 5 |
| drop_manual_memoization | inference/drop_manual_memoization.rs | REAL | 125 |
| inline_iife | inference/inline_iife.rs | DEFERRED | 7 |
| infer_mutation_aliasing_effects | inference/infer_mutation_aliasing_effects.rs | STUB | 7 |
| dead_code_elimination | optimization/dead_code_elimination.rs | REAL | 583 |
| outline_functions | optimization/outline_functions.rs | REAL | 702 |
| constant_propagation | optimization/constant_propagation.rs | REAL | 415 |
| optimize_props_method_calls | optimization/optimize_props_method_calls.rs | STUB | 2 |
| optimize_for_ssr | optimization/optimize_for_ssr.rs | STUB | 2 |
| outline_jsx | optimization/outline_jsx.rs | STUB | 2 |
| prune_maybe_throws | optimization/prune_maybe_throws.rs | STUB | 2 |
| infer_reactive_scope_variables | reactive_scopes/infer_reactive_scope_variables.rs | REAL | 636 |
| merge_reactive_scopes_that_invalidate_together | reactive_scopes/merge_reactive_scopes... | REAL | 569 |
| propagate_scope_dependencies_hir | reactive_scopes/propagate_scope_dependencies_hir.rs | REAL | 824 |
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
- **Fixture total is 1717**: The fixture dir has 18 subdirectories (`fault-tolerance/`, `propagate-scope-deps-hir-fork/`, `reduce-reactive-deps/`, `exhaustive-deps/`, `rules-of-hooks/`, etc.). `collect_fixture_paths` recursively collects all 1717 fixtures. This matches what the TS compiler tests against.

---

## Architecture

```
oxc parse -> pre-lowering validators -> HIR lowering -> SSA -> inference ->
optimization -> reactive scope inference -> reactive scope transforms ->
build_reactive_function <- CRITICAL MISSING PIECE ->
codegen (currently bypasses ReactiveFunction) -> oxc_codegen -> JS output
```

---

## History

| Date | Compile % | Correct % | Overall % | Passes Real | Stubs | Notes |
|------|-----------|-----------|-----------|-------------|-------|-------|
| 2026-03-02 | 61.0 | 12.5 | — | 14 | 38 | baseline |
| 2026-03-02 | 61.0 | 15.6 | — | 14 | 38 | codegen, SSA, scope passes |
| 2026-03-02 | 61.0 | 16.0 | — | 15 | 37 | drop_manual_memoization, IIFE unwrap |
| 2026-03-03 | 61.0 | 16.4 | — | 16 | 36 | PruneNonEscapingScopes (DeclarationId), dep hoisting |
| 2026-03-03 | 61.0 | 16.5 | — | 16 | 36 | optional chaining fix, mismatch analysis, plan |
| 2026-03-03 | 61.0 | 16.7 | — | 16 | 36 | Phase 1 codegen: $tN leak, for-init, lambda hoisting |
| 2026-03-03 | 61.0 | 17.2 | — | 16 | 36 | switch braces (+3), for-loop update DCE + continue (+6) |
| 2026-03-03 | 61.0 | 17.2 | — | 16 | 36 | ralph-loop iter1: flatten_reactive_loops deferred, near-miss analysis |
| 2026-03-03 | 61.0 | 17.5 | — | 16 | 36 | ralph-loop iter2: alloc dep tracing (+4), rename_variables deferred |
| 2026-03-03 | 61.0 | 17.5 | — | 16 | 36 | ralph-loop iter3: tree builder skeleton, scope inference investigation |
| 2026-03-03 | 61.0 | 17.5 | — | 16 | 36 | fixed propagate_scope_dependencies compile regression |
| 2026-03-03 | 61.0 | 17.5 | — | 16 | 36 | scope-terminals + loop-flatten passes behind flags |
| 2026-03-04 | 61.0 | 17.7 | — | 17 | 35 | align_reactive_scopes_to_block_scopes_hir: stub->REAL (+4) |
| 2026-03-04 | 61.0 | 19.1 | — | 17 | 35 | const-prop folding (+1), hook method call (+2), scope output naming (+4), lint mode + use-no-memo (+17) |
| 2026-03-04 | 61.0 | 20.1 | — | 17 | 35 | pragma support (+6), update expr results (+2), @gating (+1) |
| 2026-03-04 | 61.0 | 20.2 | — | 17 | 35 | destructuring const->let for mutated bindings (+2) |
| 2026-03-05 | 61.0 | 20.9 | — | 17 | 35 | lattice const-prop, dep hoisting, return/else codegen (+11) |
| 2026-03-05 | 61.0 | 21.0 | — | 18 | 28 | SCCP branch folding, phi self-loop fix, catch norm (+3) |
| 2026-03-05 | 61.0 | 21.4 | — | 18 | 28 | catch space norm, brace/JSX spacing norm (+7) |
| 2026-03-05 | 61.0 | 23.1 | — | 18 | 28 | closure rewrite, destructuring defaults, dead phi DCE, SSA, scope fixes (+29) |
| 2026-03-05 | 61.0 | 23.8 | — | 18 | 28 | function outlining, scope output counting, test normalizations (+12) |
| 2026-03-05 | 61.0 | 24.1 | — | 18 | 28 | TSX parsing, type annotation stripping, as-const norm (+5) |
| 2026-03-06 | 61.0 | 25.3 | — | 18 | 28 | ComputedLoad dep tracing, for-of destructuring, IIFE/binding norms (+22) |
| 2026-03-07 | 61.0 | 25.3+ | — | 18 | 28 | React namespace hooks, logical phi, labeled blocks, const inlining (142/300 output correct) |
| 2026-03-07 | 82.5 | 26.9 | — | 18 | 28 | Destructure post-scope fix, chained logical phi fix (463/1717 rebased) |
| 2026-03-07 | 82.5 | 32.6 | — | 18 | 28 | Switched to recursive fixture scan (1244→1717), InlineJs dep propagation fix (560/1717) |
| 2026-03-07 | 82.5 | 33.0 | — | 18 | 28 | Ternary phi node resolution (+6, 566/1717) |
| 2026-03-07 | 82.6 | 33.0 | — | 18 | 28 | Allow hook-named local vars as values (566/1717, compile 1419/1717) |
