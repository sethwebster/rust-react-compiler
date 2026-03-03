# Plan: Correctness 300 → 500+ (24.1% → 40%+)

**Created**: 2026-03-03
**Baseline**: 300/1244 correct (24.1%), 1048/1244 compile (84.2%), 0 unexpected errors

## Where We Are

282 → 300 (+18) from codegen fixes and dep tracing. The remaining 748 mismatches
overwhelmingly need the **ReactiveFunction tree** — a tree representation that
replaces the flat CFG for codegen. Without it:

- `rename_variables` can't do block-scoped naming (collides with codegen temps)
- Early returns inside scopes can't be codegen'd (need labeled blocks)
- Nested scopes can't be codegen'd correctly
- 5+ downstream passes are stuck as stubs

**The ReactiveFunction tree is the critical path.** Everything else is incremental.

## What's Done (Phases 1-2)

| Fix | Gain |
|-----|------|
| Optional chaining dep bridging | +2 |
| `ident_name` FunctionExpression name_hint | +1 |
| For-loop init reassembly | +1 |
| Lambda hoisting (pipeline reorder + DCE) | +1 |
| Switch case braces | +3 |
| For-loop update DCE + continue suppression | +6 |
| Object/Array allocation dep tracing | +4 |
| **Total** | **+18** |

## What's Blocked Without ReactiveFunction

| Pass | Status | Why blocked |
|------|--------|-------------|
| `rename_variables` | STUB | Flat renaming collides with codegen scope temps |
| `flatten_reactive_loops` | STUB | Can't distinguish scope-inside-loop from scope-wrapping-loop |
| `prune_always_invalidating_scopes` | STUB | Needs `scope.dependencies` + tree walk |
| `propagate_early_returns` | STUB | Needs to mark scope tree nodes |
| `codegen_reactive_function` | STUB | The tree codegen itself |
| Named identifier in memo blocks | Blocked | Needs coordinated scope tree + codegen |

## Phase 3: Build the ReactiveFunction Tree (target: 300 → 400+)

**This is the make-or-break phase.** 3 pieces, ~2500 LOC Rust total.

### 3a. `build_reactive_scope_terminals_hir` (~200 LOC)

**What**: Insert `Terminal::Scope` nodes into the flat CFG wherever a reactive
scope boundary exists. This converts scope ranges (stored in `env.scopes`) into
actual CFG structure.

**TS source**: `BuildReactiveScopeTerminalsHIR.ts` (~300 LOC)

**Why first**: `build_reactive_function` consumes these scope terminals to know
where to create `ReactiveScopeBlock` nodes in the tree.

### 3b. `build_reactive_function` (~800-1000 LOC)

**What**: Convert flat CFG (blocks + terminals) → `ReactiveBlock` tree.

**TS source**: `BuildReactiveFunction.ts` (1486 LOC)

**Algorithm**:
1. Start at entry block
2. Walk instructions sequentially, emitting `ReactiveInstruction` nodes
3. When hitting a scope terminal → wrap subsequent instructions in `ReactiveScopeBlock`
4. When hitting if/loop/switch terminals → recurse into branches, build sub-trees
5. Handle terminals that cross scope boundaries by splitting/nesting
6. Reconstruct `ReactiveValue` variants (Logical, Ternary, Sequence) from the
   flat Logical/Ternary/Optional terminals

**Hard parts**:
- Scope boundaries that start mid-block or cross terminal boundaries
- Labeled break/continue targets that reference outer scopes
- Phi resolution (SSA phis become assignments in the tree)

**Types**: Already defined in `hir.rs` lines 1306-1425 (`ReactiveBlock`,
`ReactiveStatement`, `ReactiveScopeBlock`, `ReactiveTerminal`, etc.)

### 3c. `codegen_reactive_function` (~1200-1500 LOC)

**What**: Walk `ReactiveBlock` tree → JS output. Replaces the body-emission
portion of `hir_codegen.rs`.

**TS source**: `CodegenReactiveFunction.ts` (2479 LOC)

**Algorithm** (recursive tree walk):
- `ReactiveInstruction` → emit JS statement
- `ReactiveScopeBlock` → emit `if ($[N] !== dep) { ...body; $[N] = dep; $[M] = out; } else { out = $[M]; }`
- `ReactiveTerminal::If` → emit `if (test) { ...consequent } else { ...alternate }`
- `ReactiveTerminal::For` → emit `for (init; test; update) { ...body }`
- `ReactiveTerminal::Return` → emit `return value;`
- Labels → emit `bb0: { ... break bb0; }` for early returns through scopes

**Strategy**: Dual codegen. Keep current `hir_codegen.rs` as fallback. Use new
tree codegen when `build_reactive_function` succeeds. This protects the existing
300 correct fixtures while unlocking new ones.

### Expected gains

| Piece | Unlocks |
|-------|---------|
| Scope terminals + tree build | Correct scope nesting, labeled breaks |
| Tree codegen (scopes) | Correct `if/else` memo blocks with proper deps |
| Tree codegen (early returns) | `bb0: { if (...) { ...; break bb0; } }` pattern |
| `rename_variables` on tree | Correct `t0`/`t1` naming without collisions |

**Conservative estimate**: +100-150 fixtures from tree codegen alone.
**Optimistic**: +200 if scope boundaries are mostly correct already.

## Phase 4: Polish on Tree (target: 400 → 500+)

Once the tree exists, these passes become implementable:

### 4a. `rename_variables` on ReactiveFunction
Block-scoped naming with proper collision avoidance. The TS implementation is
190 LOC — straightforward once the tree exists.

### 4b. `flatten_reactive_loops`
Walk tree, convert `Scope` inside loop body → `PrunedScope`. 70 LOC in TS.

### 4c. `prune_always_invalidating_scopes`
Walk tree, check scope deps for always-invalidating values. 120 LOC in TS.

### 4d. Named identifier in memo blocks
With tree codegen, scope outputs are naturally named by the tree structure
instead of the flat `analyze_scope` + `is_named_var` hack.

### 4e. Scope merging improvements
Better adjacent/overlapping merge with tree-aware boundaries.

## Execution Order

```
3a. build_reactive_scope_terminals_hir  (gate)
3b. build_reactive_function             (gate)
3c. codegen_reactive_function           (the big unlock)
  → test, commit gains
4a. rename_variables on tree
4b. flatten_reactive_loops on tree
4c. prune_always_invalidating_scopes on tree
4d-e. polish
```

3a-3c are sequential (each depends on the previous). 4a-4e are independent.

## Verification

```bash
cargo test --test fixtures run_all_fixtures -- --ignored --nocapture 2>&1 | grep 'Output correct:'
```

## Risk Mitigation

- **Dual codegen**: tree codegen is opt-in. If it produces worse output for a
  fixture, fall back to flat codegen. No regression risk.
- **Incremental tree build**: start with simple cases (single scope, no nesting,
  no early returns). Add complexity incrementally.
- **Types already exist**: `ReactiveBlock`, `ReactiveStatement`, etc. are defined
  in `hir.rs`. No type design needed.
