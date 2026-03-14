# Agent Workspace: rust-react-compiler

This is a Rust port of Meta's React compiler — originally ~56,000 lines of TypeScript across 55 passes. You are working in a live, incomplete implementation. Read this file carefully before touching any code. It will save you from re-deriving things that took hours to learn.

Also read [AGENT-STATE.md](./AGENT-STATE.md), which tracks live session metrics, the current task, and what's blocked. Update it at the end of every session without exception.

---

## Fix Methodology (mandatory — follow this every time)

**Rule: one pass at a time. One fixture at a time.**

Do not attempt broad multi-pass fixes. Do not batch-fix many fixtures at once. Each fix cycle is:

1. **Pick one failing fixture** from the mismatch list
2. **Get TS ground truth** — dump the TS compiler's HIR at every pass:
   ```bash
   node /home/claude-code/development/rust-react-compiler/dump_ts_hir.js \
     react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler/<fixture>
   # Output: /tmp/ts_hir/<fixture>/<PassName>.txt for every pass
   ```
3. **Get Rust output** for the same fixture:
   ```bash
   FIXTURE="<fixture>.js" cargo test --test fixtures fixture_print_single -- --nocapture 2>&1 | tail -40
   ```
4. **Find the first diverging pass** — compare TS HIR at each pass against what the Rust pass should produce. Key passes to check in order:
   - `InferReactiveScopeVariables.txt` — scope boundaries correct?
   - `MergeOverlappingReactiveScopesHIR.txt` — scopes merged correctly?
   - `PropagateScopeDependenciesHIR.txt` — deps propagated correctly?
   - `RenameVariables.txt` — final structure before codegen?
5. **Fix only the one diverging pass** — read the TS source for that pass, fix the Rust port
6. **Verify the single fixture passes**, then run the full suite to count improvement
7. **Commit** the fix with count: `fix: <pass> — <description> (+N, M/1717)`
8. **Repeat** from step 1 with the next fixture

### Why one at a time?

Multi-fixture fixes tend to over-fit to the specific cases examined and break others. A single-pass fix that's correct will automatically fix all fixtures that hit that same pass bug. The suite score is the signal — trust it.

### TS HIR dump script

`/home/claude-code/development/rust-react-compiler/dump_ts_hir.js` — uses the pre-built compiler at `/home/claude-code/development/pepper/node_modules/babel-plugin-react-compiler/`. Works for `.js`, `.ts`, `.tsx` fixtures. Extension is auto-detected.

---

## Session Protocol

### On Session Start (mandatory, in order)

```bash
# 1. Load push credentials (for real-time status updates to the dashboard)
set -a && source rust-react-compiler/.env && set +a

# 2. Check where the work left off
cat AGENT-STATE.md

# 3. Verify git state — uncommitted changes are common between sessions
git log --oneline -10
git diff HEAD --stat

# 4. Get your baseline metrics BEFORE touching any code
cd rust-react-compiler && cargo test --test fixtures run_all_fixtures -- --ignored 2>&1 | grep -E "Compile rate|Correct rate|Error"
```

Do not begin any implementation work until you have the baseline metrics. You need them to measure whether your changes helped.

### Real-Time Status Push

After sourcing `.env`, push live status updates to https://isreactcompilerrustyet.com:

```bash
# What you're working on (viewers see this immediately)
bash rust-react-compiler/scripts/push-status.sh status "Implementing build_reactive_function"

# After a metric change (updates compile/correct rate cards live)
bash rust-react-compiler/scripts/push-status.sh progress "Fixed for-loop codegen" 86.5 24.2

# After a significant win (triggers emoji celebration for all viewers)
bash rust-react-compiler/scripts/push-status.sh milestone "build_reactive_function passing 40 fixtures"
```

Push a `status` update when you start a new task. Push `progress` after running the fixture suite and seeing metric changes. Push `milestone` for breakthroughs (5+ new fixtures passing, new pass implemented, etc.).

### On Session End (mandatory)

Update the following sections in `AGENT-STATE.md` before stopping:

- **Metrics** — current compile rate, correct rate, error counts
- **Current Task** — what the next agent should start on
- **Completed This Session** — concrete list of files changed and what changed
- **Todo List** — cross off completed items (`- [ ]` → `- [x]`), add new tasks; this is the canonical backlog
- **Blocked On** — current blockers
- **Key Invariants** — anything you had to re-derive that wasn't written down
- **History** — append one row with current metrics

**Do not add a `## Next 3 Actions` section** — that's been replaced by `## Todo List`.

The `## Todo List` section is displayed live at https://rust-react-compiler.sethwebster.workers.dev. Maintain it throughout your session, not just at the end:
- Session start: review the list, mark your item `→ in progress`
- During: cross off items as you complete them
- Session end: add newly discovered tasks

Format:
```
## Todo List
- [x] Fix destructured parameter lowering
- [ ] Define ReactiveFunction / ReactiveScope types in hir.rs
- [ ] Implement build_reactive_function
```

If you skip this, the next agent starts blind.

---

## Repository Layout

```
react-compiler-bun/
├── AGENT-STATE.md              # Live session state — read first, update last
├── AGENT-WORKSPACE.md          # This file
├── AGENTS.md                   # Agent orchestration guidelines
├── CLAUDE.md                   # Project-level coding standards
│
├── rust-react-compiler/        # The Rust compiler crate
│   ├── Cargo.toml              # Dependencies: oxc 0.69, petgraph, indexmap, serde
│   ├── src/
│   │   ├── main.rs             # CLI entry point: react-compiler <file>
│   │   ├── lib.rs              # Crate root, module declarations
│   │   ├── error.rs            # CompilerError, CompilerDiagnostic, Result type
│   │   ├── entrypoint/
│   │   │   └── pipeline.rs     # Top-level compile() fn — orchestrates all passes
│   │   ├── hir/
│   │   │   ├── hir.rs          # All HIR types: Place, Instruction, BasicBlock, HIRFunction, …
│   │   │   ├── environment.rs  # Environment, EnvironmentConfig — identifier arena + config
│   │   │   ├── build_hir.rs    # lower_program(), lower_program_nth() — oxc AST → HIR
│   │   │   ├── print_hir.rs    # Debug printer for HIR
│   │   │   ├── types.rs        # Type system types
│   │   │   ├── visitors.rs     # HIR visitor infrastructure
│   │   │   └── lower/
│   │   │       ├── core.rs     # Top-level lowering + pre-lowering validators
│   │   │       ├── expressions.rs
│   │   │       ├── functions.rs
│   │   │       ├── calls.rs
│   │   │       ├── control_flow.rs
│   │   │       ├── loops.rs
│   │   │       ├── jsx.rs
│   │   │       ├── patterns.rs
│   │   │       └── properties.rs
│   │   ├── ssa/
│   │   │   ├── enter_ssa.rs                   # REAL — SSA construction
│   │   │   ├── eliminate_redundant_phi.rs      # REAL — phi pruning
│   │   │   └── rewrite_instruction_kinds.rs    # REAL — reassignment rewrite
│   │   ├── inference/
│   │   │   ├── aliasing_effects.rs             # REAL
│   │   │   ├── infer_mutation_aliasing_ranges.rs  # REAL
│   │   │   ├── infer_reactive_places.rs        # REAL
│   │   │   ├── analyse_functions.rs            # STUB
│   │   │   ├── drop_manual_memoization.rs      # STUB
│   │   │   ├── inline_iife.rs                  # STUB
│   │   │   └── infer_mutation_aliasing_effects.rs # STUB
│   │   ├── type_inference/
│   │   │   └── infer_types.rs                  # PARTIAL
│   │   ├── optimization/
│   │   │   ├── dead_code_elimination.rs        # REAL
│   │   │   ├── outline_functions.rs            # REAL
│   │   │   ├── constant_propagation.rs         # PARTIAL
│   │   │   ├── optimize_props_method_calls.rs  # STUB
│   │   │   ├── optimize_for_ssr.rs             # STUB
│   │   │   ├── outline_jsx.rs                  # STUB
│   │   │   └── prune_maybe_throws.rs           # STUB
│   │   ├── reactive_scopes/
│   │   │   ├── infer_reactive_scope_variables.rs        # REAL (540 LOC)
│   │   │   ├── merge_reactive_scopes_that_invalidate_together.rs # REAL (441 LOC)
│   │   │   ├── propagate_scope_dependencies_hir.rs      # REAL (274 LOC)
│   │   │   ├── merge_overlapping_reactive_scopes_hir.rs # REAL (125 LOC)
│   │   │   ├── prune_unused_scopes.rs                   # REAL (180 LOC)
│   │   │   ├── promote_used_temporaries.rs              # REAL
│   │   │   ├── prune_non_reactive_dependencies.rs       # PARTIAL (15 LOC)
│   │   │   ├── build_reactive_function.rs               # STUB (2 LOC) *** CRITICAL ***
│   │   │   ├── codegen_reactive_function.rs             # STUB (14 LOC)
│   │   │   ├── rename_variables.rs                      # STUB
│   │   │   └── [18 other stub files]
│   │   ├── validation/
│   │   │   ├── validate_hooks_usage.rs                  # PARTIAL
│   │   │   ├── validate_no_ref_access_in_render.rs      # PARTIAL
│   │   │   └── [11 other stub files]
│   │   ├── transform/
│   │   │   └── name_anonymous_functions.rs              # STUB
│   │   ├── codegen/
│   │   │   └── hir_codegen.rs                           # Operates on HIR directly (not ReactiveFunction)
│   │   └── utils/
│   │       └── merge_consecutive_blocks.rs
│   └── tests/
│       └── fixtures.rs         # Test harness: 1,244 fixtures, compile + correct rate tracking
│
├── react/                      # ZOMBIE GIT SUBMODULE — do not rm -rf, do not trust
│   └── compiler/packages/babel-plugin-react-compiler/src/
│       ├── __tests__/fixtures/compiler/  # 1,244 fixture inputs + .expect.md files
│       └── [TypeScript reference source — the compiler being ported]
│
├── is-port-done-yet/           # Cloudflare Worker progress dashboard
│   ├── server.ts               # Local dev: bun is-port-done-yet/server.ts → localhost:3420
│   └── worker.ts               # Deployed to Cloudflare, fetches AGENT-STATE.md every 3s
│
└── output/                     # Scratch output directory
```

---

## Key Commands

```bash
# Build
cd rust-react-compiler && cargo build

# Run on a single file
cd rust-react-compiler && cargo run -- path/to/file.jsx

# Full fixture test suite (slow — ~1,244 fixtures)
cd rust-react-compiler && cargo test --test fixtures run_all_fixtures -- --ignored 2>&1 | grep -E "Compile rate|Correct rate|Error"

# Full output with per-fixture details
cd rust-react-compiler && cargo test --test fixtures run_all_fixtures -- --ignored --nocapture 2>&1 | tail -50

# Fast compile check only (no tests)
cd rust-react-compiler && cargo check

# Local progress dashboard
bun is-port-done-yet/server.ts
# Then open http://localhost:3420

# Reference TypeScript compiler location
ls react/compiler/packages/babel-plugin-react-compiler/src/
```

---

## Compilation Pipeline

Every pass runs in the order shown below. Passes marked STUB are no-ops (return immediately or return the input unchanged). The critical blocker is `build_reactive_function` — everything downstream of it produces incorrect output until it is implemented.

```
 1. oxc parse (oxc 0.69)
    └── Parses JS/TS/JSX/TSX into oxc AST

 2. run_pre_lowering_validators    [core.rs]          REAL
    └── Static checks before lowering (e.g. optional-dep mismatches,
        indirect props mutation in effects)

 3. file_should_passthrough        [pipeline.rs]       REAL
    └── Handles 'use no memo', already-compiled files, @expectNothingCompiled

 4. HIR lowering                   [hir/lower/]        REAL (incomplete)
    └── lower_program() — oxc AST → HIRFunction
        Handles: functions, expressions, control flow, JSX, loops, calls
        NOT YET: destructured parameters (in progress)

 5. enter_ssa                      [ssa/enter_ssa.rs]  REAL
    └── Renames variables to SSA form, inserts phi nodes at join points

 6. eliminate_redundant_phi        [ssa/]              REAL
    └── Removes phi nodes with a single predecessor

 7. rewrite_instruction_kinds      [ssa/]              REAL
    └── Rewrites instruction kinds based on reassignment analysis

 8. merge_consecutive_blocks       [utils/]            REAL
    └── Coalesces basic blocks with single successors

 9. infer_types                    [type_inference/]   PARTIAL
    └── Infers React-specific types (hook, component, primitive, …)

10. name_anonymous_functions       [transform/]        STUB

11. drop_manual_memoization        [inference/]        STUB

12. analyse_functions              [inference/]        STUB

13. inline_iife                    [inference/]        STUB

14. dead_code_elimination          [optimization/]     REAL

15. outline_functions              [optimization/]     REAL

16. constant_propagation           [optimization/]     PARTIAL

17. optimize_props_method_calls    [optimization/]     STUB

18. optimize_for_ssr               [optimization/]     STUB

19. outline_jsx                    [optimization/]     STUB

20. prune_maybe_throws             [optimization/]     STUB

21. infer_mutation_aliasing_ranges [inference/]        REAL
    └── Computes mutable live ranges for identifiers

22. infer_mutation_aliasing_effects [inference/]       STUB

23. infer_reactive_places          [inference/]        REAL
    └── Marks which places are reactive (depend on props/state)

24. aliasing_effects               [inference/]        REAL

25. infer_reactive_scope_variables [reactive_scopes/]  REAL (540 LOC)
    └── Groups instructions into ReactiveScope boundaries

26. align_method_call_scopes       [reactive_scopes/]  STUB

27. align_object_method_scopes     [reactive_scopes/]  STUB

28. prune_unused_labels_hir        [reactive_scopes/]  STUB

29. align_reactive_scopes_to_block_scopes_hir          STUB

30. merge_overlapping_reactive_scopes_hir              REAL (125 LOC)

31. build_reactive_scope_terminals_hir                 STUB

32. flatten_reactive_loops_hir     [reactive_scopes/]  STUB

33. flatten_scopes_with_hooks_or_use_hir               STUB

34. propagate_scope_dependencies_hir [reactive_scopes/] REAL (274 LOC)

35. merge_reactive_scopes_that_invalidate_together     REAL (441 LOC)

36. memoize_fbt_and_macro_operands [reactive_scopes/]  STUB

37. prune_non_reactive_dependencies [reactive_scopes/] PARTIAL (15 LOC)

38. prune_unused_scopes            [reactive_scopes/]  REAL (180 LOC)

39. prune_always_invalidating_scopes                   STUB

40. propagate_early_returns        [reactive_scopes/]  STUB

41. prune_unused_lvalues           [reactive_scopes/]  STUB

42. promote_used_temporaries       [reactive_scopes/]  REAL

43. extract_scope_declarations_from_destructuring      STUB

44. stabilize_block_ids            [reactive_scopes/]  STUB

45. prune_non_escaping_scopes      [reactive_scopes/]  STUB

46. prune_hoisted_contexts         [reactive_scopes/]  STUB

47. prune_unused_labels            [reactive_scopes/]  STUB

48. rename_variables               [reactive_scopes/]  STUB

49. **build_reactive_function**    [reactive_scopes/]  STUB *** CRITICAL BLOCKER ***
    └── Should: HIRFunction (post-scope) → ReactiveFunction
        ReactiveFunction TYPE DOES NOT EXIST YET in hir.rs
        TS reference: ReactiveScopes/BuildReactiveFunction.ts

50. codegen_reactive_function      [reactive_scopes/]  STUB
    └── Should operate on ReactiveFunction — currently bypassed

51. codegen (hir_codegen.rs)       [codegen/]          REAL (architectural mismatch)
    └── Operates on raw HIR directly instead of ReactiveFunction
        Correct output requires build_reactive_function first

52. oxc_codegen → JS output
```

---

## Architecture Decisions

These decisions are settled. Do not revisit them without a strong reason.

| Decision | Choice | Rationale |
|---|---|---|
| ID representation | `u32` newtypes via `opaque_id!` macro | Zero-cost, type-safe, no pointer aliasing |
| Identifier storage | `Environment.identifiers: HashMap<IdentifierId, Identifier>` | Arena pattern; `Place` stores only `IdentifierId` |
| Block ordering | `IndexMap<BlockId, BasicBlock>` in reverse-postorder | Iteration order = RPO for free |
| Lifetime strategy | No lifetimes on HIR types — all owned `String`s | Avoids borrow complexity; cloning is acceptable |
| Parser | oxc 0.69 | Not Babel — AST node shapes differ significantly |
| Serialization | `serde::Serialize/Deserialize` on all HIR types | Requires `indexmap = { features = ["serde"] }` |
| Codegen target | `ReactiveFunction` (not yet implemented) | Mirrors TS architecture — current HIR-direct codegen is temporary |
| Parallelism | Single-threaded per-file | Rayon deferred; keep it simple for now |
| TS support | Full from day one | oxc parses TS/TSX natively — no extra work needed |

---

## TypeScript Reference Compiler Mapping

When implementing a pass, find the corresponding TS file here:

```
react/compiler/packages/babel-plugin-react-compiler/src/
├── HIR/
│   ├── HIR.ts                          → hir/hir.rs
│   ├── Environment.ts                  → hir/environment.rs
│   ├── BuildHIR.ts                     → hir/build_hir.rs + hir/lower/
│   └── PrintHIR.ts                     → hir/print_hir.rs
├── SSA/
│   ├── EnterSSA.ts                     → ssa/enter_ssa.rs
│   └── EliminateRedundantPhi.ts        → ssa/eliminate_redundant_phi.rs
├── Inference/
│   ├── InferMutationAliasingRanges.ts  → inference/infer_mutation_aliasing_ranges.rs
│   ├── InferReactivePlaces.ts          → inference/infer_reactive_places.rs
│   └── AnalyseFunctions.ts             → inference/analyse_functions.rs
├── Optimization/
│   ├── DeadCodeElimination.ts          → optimization/dead_code_elimination.rs
│   └── OutlineFunctions.ts             → optimization/outline_functions.rs
├── ReactiveScopes/
│   ├── BuildReactiveFunction.ts        → reactive_scopes/build_reactive_function.rs *** NEXT ***
│   ├── InferReactiveScopeVariables.ts  → reactive_scopes/infer_reactive_scope_variables.rs
│   ├── PropagateEarlyReturns.ts        → reactive_scopes/propagate_early_returns.rs
│   └── RenameVariables.ts              → reactive_scopes/rename_variables.rs
├── Validation/
│   └── Validate*.ts                    → validation/validate_*.rs
└── Entrypoint/
    └── Pipeline.ts                     → entrypoint/pipeline.rs
```

---

## Test Fixture Format

Fixtures live in:

```
react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler/
```

Each fixture is a JS/TS/JSX/TSX file. For non-error fixtures, there is a companion `.expect.md` file containing the expected compiler output.

### Pass Criteria

**Compile:** The compiler must not panic or return an unexpected error.

**Correct:** The `## Code` section of the `.expect.md` file must match our output (after normalization — whitespace collapsed, trailing commas stripped, comments stripped).

### Error Fixtures

Files named `error.*` or `todo.error.*` are expected to fail compilation. These count toward the "expected errors" total. A fixture in this set that compiles successfully is an `error_unexpected` — the worst kind of regression, because it means we're silently accepting invalid React code.

Current status: **0 error_unexpected**. Do not introduce any.

### Running a Single Fixture Manually

```bash
cd rust-react-compiler && cargo run -- \
  ../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler/YOUR_FIXTURE.jsx
```

---

## Current Status

| Metric | Value |
|---|---|
| Compile rate | 84.2% (1,048 / 1,244) |
| Correct rate | 17.3% |
| Expected errors | 196 |
| Unexpected errors | 0 |

### What "Compile rate" Means

The compiler successfully processes the file without panicking or returning an internal error. It does not mean the output is correct.

### What "Correct rate" Means

The normalized JS output matches the normalized expected output in the `.expect.md` file. This is the number that matters for shipping quality. At 17.3%, we are producing correct output for simple cases but failing on anything requiring `ReactiveFunction`-aware codegen.

### Critical Path to Improving Correct Rate

The correct rate ceiling is determined by `build_reactive_function`. Until that function exists and produces a proper `ReactiveFunction`, the codegen operates on raw HIR and cannot produce the useMemo/useCallback wrapping that the expected output requires.

The three-step unlock:

1. Define `ReactiveFunction` and `ReactiveScope` types in `hir/hir.rs`
   - Reference: `react/compiler/.../ReactiveScopes/BuildReactiveFunction.ts`
2. Implement `build_reactive_function` in `reactive_scopes/build_reactive_function.rs`
   - Input: `&HIRFunction` (post scope inference)
   - Output: `ReactiveFunction`
3. Rewrite `codegen_reactive_function` (and `hir_codegen.rs`) to operate on `ReactiveFunction`

---

## Known Infrastructure Gotchas

### The Zombie Submodule

`react/` is a git submodule that is not properly initialized. It exists on disk and has the fixture files, but submodule commands may behave unexpectedly.

**Do not** run `git submodule update` or `git submodule sync`.
**Do not** run `rm -rf react/`.
**Do** use the files directly — they are on disk and readable.

In any GitHub Actions CI, always use `submodules: false` in the checkout step.

### `ReactiveFunction` Does Not Exist

As of the last session, `ReactiveFunction` is not defined anywhere in the Rust codebase. `build_reactive_function.rs` is a 2-line stub. Code that references `ReactiveFunction` will not compile.

Do not reference `ReactiveFunction` in any pass until it is defined in `hir/hir.rs`.

### Codegen Is an Architectural Mismatch

`codegen/hir_codegen.rs` operates on `HIRFunction` directly, not on `ReactiveFunction`. This is intentional — it was a deliberate temporary state to maintain a non-zero correct rate while `build_reactive_function` is unimplemented.

Once `build_reactive_function` exists, `hir_codegen.rs` should be replaced by a proper `codegen_reactive_function` that understands scope boundaries and emits `useMemo`/`useCallback` wrapping.

### oxc AST Shapes Are Not Babel Shapes

The TS reference compiler uses Babel AST. oxc 0.69 uses its own AST. Node names are often similar but field names differ. When porting a lowering pass, verify field names against the oxc 0.69 source or docs — do not assume they match Babel.

One concrete example: `WhileStatement.body` in oxc is `Statement<'a>` directly — use `&while_stmt.body`, not `.body.as_ref()`.

### The Progress Dashboard

The `is-port-done-yet/` Cloudflare Worker reads `AGENT-STATE.md` from the GitHub raw URL every 3 seconds. For the dashboard to reflect current state, `AGENT-STATE.md` must be committed and pushed. The deployed dashboard is at the Cloudflare Workers URL; the local version runs at `localhost:3420`.

---

## Key Invariants (Do Not Re-Derive)

- **Identifiers** are stored by `IdentifierId` (u32 newtype), not by reference. Use `env.identifier(id)` to look up. Storing `&Identifier` will fight the borrow checker.
- **Blocks** are in `IndexMap<BlockId, BasicBlock>` in **reverse-postorder**. Iterating the map gives you RPO traversal for free.
- **Place** stores `IdentifierId`, not a pointer to `Identifier`. All identifier data lives in `Environment.identifiers`.
- **No lifetimes on HIR types** — all owned `String`s. Avoids borrow complexity at the cost of cloning. This is intentional.
- **serde on all HIR types** requires `indexmap = { features = ["serde"] }` in `Cargo.toml`. It is already there. Do not remove it.
- **`ReactiveFunction` does not exist** — see above. Do not reference it until it is defined.
- **0 error_unexpected is a hard invariant.** If your changes introduce unexpected compilation of error fixtures, investigate immediately before committing.
- **The fixture dir path** is relative to the Cargo manifest: `"/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"` (see `tests/fixtures.rs` line 7–10).
