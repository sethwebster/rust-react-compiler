# Ralph Loop Playbook: 296 → 1048 correct

**Goal**: Close the 752-fixture gap between "compiles" and "correct output"
**Current**: 296/1244 correct (23.8%), 1048 compile, 196 expected errors

## Gap Anatomy

752 mismatches. Average fixture has 4.4 overlapping issues. Root causes cluster into 6 tiers:

| Tier | Root Cause | Fixtures Affected | Blocked By |
|------|-----------|-------------------|------------|
| **T1** | Scope dep tracking wrong (cache size, dep checks) | ~440 | T2, T3 |
| **T2** | Scope over-creation / over-memoization (sentinel_over) | ~175 | T3 |
| **T3** | Scope alignment + merging | ~360 | — |
| **T4** | `rename_variables` / `$tN` naming | ~97 | — |
| **T5** | Early return sentinel / labeled blocks | ~400+ | T6 |
| **T6** | `build_reactive_function` + reactive codegen | ~400+ | — |

**Critical insight**: T5 and T6 are the same blocker. The TS compiler uses `ReactiveFunction` (a tree of scoped blocks) for codegen. Our Rust port bypasses this and codegens from raw HIR. ~400 fixtures need the reactive function tree to produce correct output because they involve early returns inside scopes, nested scopes, or labeled break targets.

## Execution Phases

### Phase A: Foundation Passes (unblocks everything else)

**Goal**: Get the reactive scope pipeline structurally correct.

#### A1. `align_reactive_scopes_to_block_scopes_hir` (STUB → REAL)
- **What**: Adjusts scope boundaries to align with JS block scoping rules
- **TS source**: `AlignReactiveScopesToBlockScopesHIR.ts` (~200 LOC)
- **Why first**: Wrong scope boundaries cascade into wrong deps, wrong cache sizes, wrong codegen
- **Test**: Run fixtures, check if `if_block_count_diff` drops

#### A2. `flatten_scopes_with_hooks_or_use_hir` (STUB → REAL)
- **What**: Scopes containing hook calls must be flattened (hooks can't be inside conditionals)
- **TS source**: `FlattenScopesWithHooksOrUseHIR.ts` (~100 LOC)
- **Why**: Prevents illegal memoization around hook calls
- **Test**: Check `extra_memo` count drops

#### A3. `flatten_reactive_loops_hir` (STUB → REAL)
- **What**: Reactive scopes that span loop boundaries get flattened
- **TS source**: `FlattenReactiveLoopsHIR.ts` (~80 LOC)
- **Why**: Loop-spanning scopes produce wrong output
- **Test**: For-loop and while-loop fixtures improve

#### A4. `prune_always_invalidating_scopes` (STUB → REAL)
- **What**: Scopes whose deps change every render are useless — prune them
- **TS source**: `PruneAlwaysInvalidatingScopes.ts` (~60 LOC)
- **Why**: Reduces `sentinel_over` count
- **Test**: Check sentinel count drops

#### A5. `prune_non_reactive_dependencies` (PARTIAL → REAL)
- **What**: Remove deps from scopes that are provably non-reactive (constants, globals)
- **TS source**: `PruneNonReactiveDependencies.ts` (~150 LOC)
- **Why**: Wrong deps → wrong dep checks in output
- **Test**: `dep_check_count_diff` drops

**Expected gain from Phase A**: +80-150 fixtures (scope boundaries closer to correct)

### Phase B: Naming + Codegen Polish

#### B1. `rename_variables` (STUB → REAL)
- **What**: Assign sequential `t0`, `t1` names to promoted temporaries; resolve naming conflicts
- **TS source**: `RenameVariables.ts` (~190 LOC)
- **Approach**: Walk all identifiers by DeclarationId. Promoted temps (`#tN`) → `t0`, `t1`, etc. Named vars with conflicts → `name$0`, `name$1`.
- **Collision avoidance**: Must coordinate with codegen's `scope_index` counter. Set `param_name_offset = num_promoted_temps` in codegen.
- **Test**: `$tN` disappears from all output

#### B2. `promote_used_temporaries` (expand beyond params)
- **What**: Currently only promotes unnamed params. Must also promote scope output temps.
- **TS source**: `PromoteUsedTemporaries.ts` (~100 LOC)
- **Why**: Without promotion, scope outputs stay unnamed → codegen allocates `tN` temps independently
- **Test**: `const_alias` count drops (no more `const arr = t0;`)

#### B3. `extract_scope_declarations_from_destructuring` (STUB → REAL)
- **What**: Destructured bindings inside scopes need special handling
- **TS source**: ~80 LOC
- **Why**: Destructured props/state are common in React
- **Test**: Destructuring fixtures improve

**Expected gain from Phase B**: +30-60 fixtures

### Phase C: The Big One — Reactive Function + Codegen

This is the 400+ fixture unlock. The TS compiler converts HIR → ReactiveFunction (a tree) → output JS. We skip the tree and codegen from flat HIR. This works for simple cases but fails for:
- Early returns inside scopes (need labeled blocks + sentinel)
- Nested scopes (inner scope's cache slots shift outer scope's)
- Scope-spanning control flow (if/else where one branch is scoped)

#### C1. Define `ReactiveFunction` / `ReactiveScope` types in `hir.rs`
- Add the tree types: `ReactiveBlock`, `ReactiveScopeBlock`, `ReactiveValue`
- These already exist partially in `hir.rs` but are unused

#### C2. `build_reactive_function` (STUB → REAL)
- **What**: Convert flat HIR + scope info → tree of ReactiveBlocks
- **TS source**: `BuildReactiveFunction.ts` (~400 LOC)
- **This is the hardest pass in the entire compiler**
- Must handle: scope nesting, early returns, break/continue through scopes, phi resolution
- **Approach**: Walk blocks in RPO. For each scope, collect its instructions into a `ReactiveScopeBlock`. Handle terminals that cross scope boundaries by inserting labeled blocks.

#### C3. `build_reactive_scope_terminals_hir` (STUB → REAL)
- **What**: Insert terminal-aware scope boundaries (early return sentinels)
- **TS source**: `BuildReactiveScopeTerminalsHIR.ts` (~300 LOC)
- **Why**: Enables `if ($[0] !== sentinel) { ... if (cond) return early; ... }`

#### C4. `propagate_early_returns` (STUB → REAL)
- **What**: Mark scopes that contain early returns
- **TS source**: `PropagateEarlyReturns.ts` (~100 LOC)

#### C5. `codegen_reactive_function` (STUB → REAL)
- **What**: Walk ReactiveFunction tree → JS output
- **TS source**: `CodegenReactiveFunction.ts` (~800 LOC)
- **Replaces**: Current `hir_codegen.rs` for the inner function body
- **Approach**: Recursive tree walk. Each `ReactiveScopeBlock` emits `if ($[N] !== dep) { ... }` or sentinel pattern.

**Expected gain from Phase C**: +200-400 fixtures (this is where the real money is)

### Phase D: Long Tail

#### D1. `constant_propagation` (PARTIAL → REAL)
- Propagate constants through phis, eliminate dead init assignments
- ~30 fixtures

#### D2. `analyse_functions` + `infer_mutation_aliasing_effects` (STUBS → REAL)
- Function effect inference — determines which calls mutate which values
- Blocks correct reactivity inference for ~50 fixtures

#### D3. Control flow codegen improvements
- `do-while` body emission
- `switch` fallthrough (no implicit break)
- Labeled `break`/`continue`
- ~20 fixtures

#### D4. Validation passes (STUBS → REAL)
- Won't change correct count, but needed for error fixture correctness
- Lower priority

## Ralph Loop Instructions

### Loop Structure

```
for each phase in [A1, A2, A3, A4, A5, B1, B2, B3, C1, C2, C3, C4, C5, D1, D2, D3]:
  1. Read the TS source for the pass
  2. Read the existing Rust stub
  3. Implement the pass in Rust
  4. cargo test (unit tests must pass)
  5. cargo test --test fixtures run_all_fixtures -- --ignored --nocapture 2>&1 | grep 'Output correct:'
  6. If gain > 0: git add + commit + push
  7. If gain < 0 (regression): investigate, fix, or revert
  8. If gain == 0: still commit if the pass is structurally correct (unlocks later phases)
  9. Update AGENT-STATE.md metrics + history
```

### Per-Step Rules

1. **Read the TS source FIRST**. Every pass has a TS reference in `react/compiler/packages/babel-plugin-react-compiler/src/`. Read it completely before writing any Rust.
2. **Match the TS behavior, not the TS code**. Rust doesn't have ReactiveFunction yet. Adapt the logic to work on HIR + Environment.
3. **One pass per commit**. Never combine passes in a single commit.
4. **Run fixtures after every pass**. The number must not go down. If it does, stop and fix before continuing.
5. **Skip validation passes** until Phase D. They don't affect correct output.
6. **Phase C is sequential** — C1 must complete before C2, C2 before C3, etc. Phases A and B are independent and can interleave.

### Bail Conditions

- If a pass takes more than 2 hours without progress → skip and move to next
- If a pass regresses by more than 5 fixtures → revert and investigate
- If `cargo test` (unit tests) fails → fix before moving on
- If compile rate drops below 84% → something broke lowering, stop immediately

### Estimated Timeline

| Phase | Passes | Effort | Expected Gain |
|-------|--------|--------|---------------|
| A (scope alignment) | 5 passes | 6-10h | +80-150 |
| B (naming/polish) | 3 passes | 3-5h | +30-60 |
| C (reactive function) | 5 passes | 15-25h | +200-400 |
| D (long tail) | 4+ passes | 5-10h | +30-80 |
| **Total** | **17 passes** | **30-50h** | **340-690** |

**Projected endpoint**: 296 + 340 = 636 (low) to 296 + 690 = 986 (high)
**Realistic target**: ~750 correct (60%) after all phases

The gap between 750 and 1048 will be fixtures with unique edge cases that need individual investigation.

## Quick Reference: TS Source Locations

All under `react/compiler/packages/babel-plugin-react-compiler/src/`:

| Pass | TS File |
|------|---------|
| align_reactive_scopes_to_block_scopes | ReactiveScopes/AlignReactiveScopesToBlockScopesHIR.ts |
| flatten_scopes_with_hooks_or_use | ReactiveScopes/FlattenScopesWithHooksOrUseHIR.ts |
| flatten_reactive_loops | ReactiveScopes/FlattenReactiveLoopsHIR.ts |
| prune_always_invalidating_scopes | ReactiveScopes/PruneAlwaysInvalidatingScopes.ts |
| prune_non_reactive_dependencies | ReactiveScopes/PruneNonReactiveDependencies.ts |
| rename_variables | ReactiveScopes/RenameVariables.ts |
| promote_used_temporaries | ReactiveScopes/PromoteUsedTemporaries.ts |
| extract_scope_declarations | ReactiveScopes/ExtractScopeDeclarationsFromDestructuring.ts |
| build_reactive_function | ReactiveScopes/BuildReactiveFunction.ts |
| build_reactive_scope_terminals | ReactiveScopes/BuildReactiveScopeTerminalsHIR.ts |
| propagate_early_returns | ReactiveScopes/PropagateEarlyReturns.ts |
| codegen_reactive_function | ReactiveScopes/CodegenReactiveFunction.ts |
| constant_propagation | Optimization/ConstantPropagation.ts |
| analyse_functions | Inference/AnalyseFunctions.ts |
| infer_mutation_aliasing_effects | Inference/InferMutationAliasingEffects.ts |
