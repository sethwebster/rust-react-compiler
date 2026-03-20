#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::{HashMap, HashSet};
use crate::hir::hir::*;
use crate::ssa::eliminate_redundant_phi::eliminate_redundant_phi;

/// Lattice value for constant propagation.
/// Top = not yet analyzed, Constant = known value, Bottom = not constant.
#[derive(Debug, Clone, PartialEq)]
enum LatticeValue {
    Top,
    Constant(PrimitiveValue),
    Bottom,
}

impl LatticeValue {
    fn meet(&self, other: &LatticeValue) -> LatticeValue {
        match (self, other) {
            (LatticeValue::Top, v) | (v, LatticeValue::Top) => v.clone(),
            (LatticeValue::Bottom, _) | (_, LatticeValue::Bottom) => LatticeValue::Bottom,
            (LatticeValue::Constant(a), LatticeValue::Constant(b)) => {
                if a == b {
                    LatticeValue::Constant(a.clone())
                } else {
                    LatticeValue::Bottom
                }
            }
        }
    }
}

/// JS truthiness for constant values.
fn is_truthy(val: &PrimitiveValue) -> bool {
    match val {
        PrimitiveValue::Boolean(b) => *b,
        PrimitiveValue::Number(n) => *n != 0.0 && !n.is_nan(),
        PrimitiveValue::String(s) => !s.is_empty(),
        PrimitiveValue::Null | PrimitiveValue::Undefined => false,
    }
}

/// Constant propagation pass with lattice-based phi resolution and branch folding.
///
/// Follows the TS compiler's SCCP approach:
///   1. Lattice analysis + rewrite constants
///   2. Fold If/Branch terminals when test is a known constant
///   3. Remove unreachable blocks, prune dead phi operands, re-run phi elimination
///   4. Loop until no more branches can be folded
pub fn constant_propagation(hir: &mut HIRFunction) {
    // Safety limit: bound the number of outer rounds to the number of blocks + 1.
    // In theory, each round removes at least one reachable If terminal, so N+1 rounds
    // suffice for N blocks. The limit guards against any unforeseen infinite-loop bug.
    let max_rounds = hir.body.blocks.len() + 1;
    let mut round = 0;
    loop {
        if round >= max_rounds {
            break;
        }
        round += 1;
        let branches_folded = constant_propagation_round(hir);
        if !branches_folded {
            break;
        }
        // Clean up graph after branch folding.
        cp_remove_unreachable_blocks(hir);
        cp_prune_dead_phi_operands(hir);
        eliminate_redundant_phi(hir);
    }
}

/// One round of constant propagation + branch folding.
/// Returns true if any branches were folded.
fn constant_propagation_round(hir: &mut HIRFunction) -> bool {
    // Per-block set of "mutable variable" place identifiers: lvalue.place.identifier of
    // Let/HoistedLet/Reassign StoreLocals within each block.
    //
    // In this SSA representation, StoreLocal.lvalue.place retains the pre-SSA "x_orig"
    // identifier (never renamed by SSA). If a block has both `let x = y` and `x = 2`,
    // both StoreLocals share x_orig. A pre-assignment LoadLocal x_orig in the same block
    // must NOT fold using the lattice value — we use this set to block such LoadLocals.
    //
    // PERFORMANCE NOTE: We only track variables in the main lattice when they are assigned
    // in 2+ distinct blocks (cross_block_mutable_ids). Same-block assignments are handled
    // by the per-block blocking + local_constants in the rewrite pass. This avoids the
    // O(N_vars × N_iters) slowdown that would occur if ALL mutable variables were tracked.
    // Build a mapping from SSA lvalue id → pre-SSA orig id for StoreLocal instructions.
    // This is needed to connect PostfixUpdate/PrefixUpdate (which use the SSA-renamed id
    // in their InstructionValue.lvalue) back to the same orig variable as the StoreLocal
    // (which retains the pre-SSA orig id in lvalue.place.identifier after SSA).
    let mut ssa_lval_to_orig: HashMap<IdentifierId, IdentifierId> = HashMap::new();
    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                if matches!(lvalue.kind, InstructionKind::Let | InstructionKind::HoistedLet | InstructionKind::Reassign) {
                    ssa_lval_to_orig.insert(instr.lvalue.identifier, lvalue.place.identifier);
                }
            }
        }
    }

    let mut block_mutable_ids: HashMap<BlockId, HashSet<IdentifierId>> = HashMap::new();
    // Count blocks per mutable variable to find cross-block assignments.
    let mut var_block_count: HashMap<IdentifierId, usize> = HashMap::new();
    let mut var_seen_blocks: HashMap<IdentifierId, BlockId> = HashMap::new();
    for block in hir.body.blocks.values() {
        let mut mutable_in_block: HashSet<IdentifierId> = HashSet::new();
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                if matches!(lvalue.kind, InstructionKind::Let | InstructionKind::HoistedLet | InstructionKind::Reassign) {
                    let id = lvalue.place.identifier;
                    mutable_in_block.insert(id);
                    // Track whether this variable appears in multiple blocks.
                    let first = var_seen_blocks.entry(id).or_insert(block.id);
                    if *first != block.id {
                        *var_block_count.entry(id).or_insert(1) += 1;
                    }
                }
            }
            // PostfixUpdate/PrefixUpdate (`i++`, `--i`, etc.) also mutate their operand.
            // In the React compiler's SSA, PostfixUpdate.lvalue.identifier (InstructionValue)
            // holds the SSA-renamed id of the variable (same id as the init StoreLocal's
            // instr.lvalue.identifier). We use ssa_lval_to_orig to map back to the orig id,
            // then track it as a cross-block mutation so the init constant isn't incorrectly
            // propagated into the loop test expression.
            if let InstructionValue::PostfixUpdate { lvalue, .. }
               | InstructionValue::PrefixUpdate { lvalue, .. } = &instr.value
            {
                if let Some(&orig_id) = ssa_lval_to_orig.get(&lvalue.identifier) {
                    mutable_in_block.insert(orig_id);
                    let first = var_seen_blocks.entry(orig_id).or_insert(block.id);
                    if *first != block.id {
                        *var_block_count.entry(orig_id).or_insert(1) += 1;
                    }
                }
            }
        }
        if !mutable_in_block.is_empty() {
            block_mutable_ids.insert(block.id, mutable_in_block);
        }
    }
    // Variables assigned in 2+ distinct blocks — safe to track in the lattice.
    let cross_block_mutable_ids: HashSet<IdentifierId> = var_block_count
        .into_iter()
        .filter(|(_, count)| *count >= 1) // ≥1 additional block beyond the first
        .map(|(id, _)| id)
        .collect();

    // Instruction lvalues of all StoreLocals for cross-block mutable variables.
    // In the React compiler's SSA, some LoadLocals in loop bodies/conditions use the
    // INIT definition's SSA id (e.g., i_init_ssa) as their place.identifier, bypassing
    // the phi node. If i_init_ssa = Const(0) in ssa_constants, those LoadLocals would
    // be incorrectly replaced. Treat these ids like block-mutable vars in the rewrite pass.
    let mut cross_block_store_lvalues: HashSet<IdentifierId> = HashSet::new();
    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                if cross_block_mutable_ids.contains(&lvalue.place.identifier) {
                    cross_block_store_lvalues.insert(instr.lvalue.identifier);
                }
            }
        }
    }

    let mut lattice: HashMap<IdentifierId, LatticeValue> = HashMap::new();

    // Initialize: all Primitive instruction results are constants.
    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            if let InstructionValue::Primitive { value, .. } = &instr.value {
                lattice.insert(instr.lvalue.identifier, LatticeValue::Constant(value.clone()));
            }
        }
    }

    // Iterate until convergence using lattice-based analysis.
    // This handles cyclic phis from loops correctly.
    // Safety limit: each variable can transition at most twice (Top→Const→Bottom),
    // so 2 * num_identifiers + 1 iterations suffice.
    let max_inner_iters = 2 * (hir.body.blocks.values().map(|b| b.instructions.len() + b.phis.len()).sum::<usize>() + 1);
    let mut inner_iter = 0;
    loop {
        if inner_iter >= max_inner_iters {
            break;
        }
        inner_iter += 1;
        let mut changed = false;

        // Process phi nodes first.
        for block in hir.body.blocks.values() {
            for phi in &block.phis {
                let mut result = LatticeValue::Top;
                for (_, operand) in &phi.operands {
                    if let Some(v) = lattice.get(&operand.identifier) {
                        result = result.meet(v);
                    }
                }
                let prev = lattice.get(&phi.place.identifier).cloned().unwrap_or(LatticeValue::Top);
                if result != prev {
                    lattice.insert(phi.place.identifier, result);
                    changed = true;
                }
            }
        }

        // Process instructions.
        for block in hir.body.blocks.values() {
            let block_muts = block_mutable_ids.get(&block.id);
            for instr in &block.instructions {
                let new_val = match &instr.value {
                    InstructionValue::Primitive { value, .. } => {
                        Some(LatticeValue::Constant(value.clone()))
                    }
                    InstructionValue::LoadLocal { place, .. } => {
                        // Block folding if x_orig is mutated IN THIS BLOCK, or if
                        // the place is a cross-block StoreLocal lvalue. In the React
                        // compiler's SSA, loop-body LoadLocals may use the init def
                        // SSA id (e.g., i_init_ssa from `let i=0`) as their place
                        // rather than the phi result. Returning its lattice value would
                        // propagate Const(0) into dependent instructions (BinaryExpr, etc.)
                        // and cause incorrect folding of loop variables in all iterations.
                        let mutable_in_this_block = block_muts
                            .map(|m| m.contains(&place.identifier))
                            .unwrap_or(false);
                        if mutable_in_this_block || cross_block_store_lvalues.contains(&place.identifier) {
                            None
                        } else {
                            lattice.get(&place.identifier).cloned()
                        }
                    }
                    InstructionValue::StoreLocal { lvalue, value, .. } => {
                        let is_const = lvalue.kind == InstructionKind::Const
                            || lvalue.kind == InstructionKind::HoistedConst;
                        // Track in lattice: Const declarations (baseline) PLUS mutable
                        // variables assigned in 2+ blocks (cross-block constant propagation).
                        // Cross-block tracking enables phi.js-style folding: when
                        // eliminate_redundant_phi removes a trivial phi (both branches assign
                        // x_orig to the same constant), LoadLocals in the join block use x_orig
                        // directly. Without cross-block tracking, lattice[x_orig] is never set
                        // and the LoadLocal can't fold.
                        let is_cross_block = cross_block_mutable_ids.contains(&lvalue.place.identifier);
                        let val = lattice.get(&value.identifier).cloned();
                        if is_const || is_cross_block {
                            // For cross-block mutable vars: if the source is unknown (val=None),
                            // treat it as Bottom — any non-constant assignment forces the variable
                            // to Bottom. Otherwise x_orig stays Const from an earlier assignment
                            // even when later assigned a non-constant value.
                            // For const declarations: only update when source is known (they are
                            // assigned once, so Top stays Top until the source resolves).
                            let effective = if is_cross_block {
                                Some(val.clone().unwrap_or(LatticeValue::Bottom))
                            } else {
                                val.clone()
                            };
                            if let Some(v) = &effective {
                                let prev = lattice.get(&lvalue.place.identifier)
                                    .cloned().unwrap_or(LatticeValue::Top);
                                let new = prev.meet(v);
                                if new != prev {
                                    lattice.insert(lvalue.place.identifier, new);
                                    changed = true;
                                }
                            }
                            // For cross-block mutable vars, also return Bottom when source
                            // is unknown. This ensures the instruction's SSA lvalue is set
                            // to Bottom in the lattice (not Top), so phi nodes that reference
                            // this lvalue correctly resolve to Bottom rather than Const.
                            if is_cross_block { effective } else { val }
                        } else {
                            None
                        }
                    }
                    // PostfixUpdate/PrefixUpdate (`i++`, `++i`, etc.) produce a
                    // new value that depends on the current variable value. Mark the
                    // updated variable (lvalue.identifier) as Bottom so that any phi
                    // node in the loop header (which has the update as a back-edge
                    // operand) correctly resolves to Bottom rather than the init constant.
                    InstructionValue::PostfixUpdate { lvalue, .. }
                    | InstructionValue::PrefixUpdate { lvalue, .. } => {
                        let prev = lattice.get(&lvalue.identifier)
                            .cloned().unwrap_or(LatticeValue::Top);
                        if prev != LatticeValue::Bottom {
                            lattice.insert(lvalue.identifier, LatticeValue::Bottom);
                            changed = true;
                        }
                        Some(LatticeValue::Bottom)
                    }
                    InstructionValue::UnaryExpression { value, operator, .. } => {
                        match lattice.get(&value.identifier) {
                            Some(LatticeValue::Constant(v)) => {
                                match fold_unary(v.clone(), operator) {
                                    Some(r) => Some(LatticeValue::Constant(r)),
                                    None => Some(LatticeValue::Bottom),
                                }
                            }
                            Some(LatticeValue::Bottom) => Some(LatticeValue::Bottom),
                            _ => None,
                        }
                    }
                    InstructionValue::BinaryExpression { left, right, operator, .. } => {
                        match (lattice.get(&left.identifier), lattice.get(&right.identifier)) {
                            (Some(LatticeValue::Constant(lv)), Some(LatticeValue::Constant(rv))) => {
                                match fold_binary(lv.clone(), rv.clone(), operator.clone()) {
                                    Some(r) => Some(LatticeValue::Constant(r)),
                                    None => Some(LatticeValue::Bottom),
                                }
                            }
                            (Some(LatticeValue::Bottom), _) | (_, Some(LatticeValue::Bottom)) => {
                                Some(LatticeValue::Bottom)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };

                if let Some(new_val) = new_val {
                    let id = instr.lvalue.identifier;
                    let prev = lattice.get(&id).cloned().unwrap_or(LatticeValue::Top);
                    let merged = prev.meet(&new_val);
                    if merged != prev {
                        lattice.insert(id, merged);
                        changed = true;
                    }
                }
            }
        }

        if !changed {
            break;
        }
    }


    // Build the final constant map from lattice values.
    let mut ssa_constants: HashMap<IdentifierId, PrimitiveValue> = lattice.iter()
        .filter_map(|(id, v)| {
            if let LatticeValue::Constant(val) = v {
                Some((*id, val.clone()))
            } else {
                None
            }
        })
        .collect();

    // Note: we do NOT globally remove mutable variable ids from ssa_constants here.
    // The rewrite pass handles per-block blocking: for blocks where x_orig is also mutated,
    // the LoadLocal only uses local_constants (not ssa_constants), preserving sequential order.
    // For blocks where x_orig is only read (e.g. join block after if/else), ssa_constants IS
    // used so the cross-block constant value propagates correctly.

    // Collect identifiers captured by closures.
    let mut closure_captured_ids: std::collections::HashSet<IdentifierId> = std::collections::HashSet::new();
    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            if let InstructionValue::FunctionExpression { lowered_func, .. }
               | InstructionValue::ObjectMethod { lowered_func, .. } = &instr.value
            {
                for ctx_place in &lowered_func.func.context {
                    closure_captured_ids.insert(ctx_place.identifier);
                }
            }
        }
    }

    // Extend ssa_constants with single-assignment mutable variables (Let/Reassign) that are
    // assigned exactly once to a known constant. These variables are not in
    // cross_block_mutable_ids (single-block). After dead-code elimination removes a branch,
    // a variable like `_b = "baz"` may become single-block; without this extension,
    // LoadLocals in other blocks can't see the constant value.
    //
    // Safety guards:
    // 1. Exclude any StoreLocal whose SSA lvalue appears as a phi operand (conditional branch).
    // 2. Exclude assignments in blocks reachable from any loop body (loop may not execute).
    {
        // Collect all SSA ids used as phi operands.
        let mut phi_operand_ids: HashSet<IdentifierId> = HashSet::new();
        for block in hir.body.blocks.values() {
            for phi in &block.phis {
                for (_, place) in &phi.operands {
                    phi_operand_ids.insert(place.identifier);
                }
            }
        }
        // Collect all blocks reachable from any loop body (BFS, stopping at loop fallthroughs).
        // Assignments in these blocks might not execute if the loop runs 0 iterations.
        let mut loop_reachable_blocks: HashSet<BlockId> = HashSet::new();
        {
            // First, collect (loop_body, fallthrough) pairs.
            let mut worklist: Vec<(BlockId, BlockId)> = Vec::new();
            for block in hir.body.blocks.values() {
                match &block.terminal {
                    Terminal::While { loop_, fallthrough, .. }
                    | Terminal::DoWhile { loop_, fallthrough, .. }
                    | Terminal::For { loop_, fallthrough, .. }
                    | Terminal::ForOf { loop_, fallthrough, .. }
                    | Terminal::ForIn { loop_, fallthrough, .. } => {
                        worklist.push((*loop_, *fallthrough));
                    }
                    _ => {}
                }
            }
            // BFS from each loop body block, stopping at the loop fallthrough.
            for (start, stop) in worklist {
                let mut queue = std::collections::VecDeque::new();
                queue.push_back(start);
                while let Some(bid) = queue.pop_front() {
                    if bid == stop || loop_reachable_blocks.contains(&bid) { continue; }
                    loop_reachable_blocks.insert(bid);
                    if let Some(block) = hir.body.blocks.get(&bid) {
                        for succ_bid in block.terminal.successors() {
                            if succ_bid != stop {
                                queue.push_back(succ_bid);
                            }
                        }
                    }
                }
            }
        }

        let mut single_assign_consts: HashMap<IdentifierId, PrimitiveValue> = HashMap::new();
        // Also track SSA lvalue ids (instr.lvalue.identifier) for single-assignment consts.
        // After eliminate_redundant_phi, LoadLocals may use the StoreLocal's SSA lvalue id
        // rather than the original pre-SSA id (lvalue.place.identifier). We need both in
        // ssa_constants so the rewrite pass can substitute constants into those LoadLocals.
        // Maps: orig_id → (ssa_lvalue_id, val). Needed to find the right SSA id to remove
        // when a second assignment of the same orig variable is encountered.
        let mut single_assign_with_ssa: HashMap<IdentifierId, (IdentifierId, PrimitiveValue)> = HashMap::new();
        let mut multi_assign: HashSet<IdentifierId> = HashSet::new();
        for (bid, block) in &hir.body.blocks {
            // Skip blocks inside loop bodies — assignments there may not execute.
            if loop_reachable_blocks.contains(bid) { continue; }
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    if matches!(lvalue.kind, InstructionKind::Let | InstructionKind::HoistedLet | InstructionKind::Reassign)
                        && !cross_block_mutable_ids.contains(&lvalue.place.identifier)
                        && !closure_captured_ids.contains(&lvalue.place.identifier)
                        // Exclude conditional assignments whose SSA lvalue feeds into a phi.
                        && !phi_operand_ids.contains(&instr.lvalue.identifier)
                    {
                        let id = lvalue.place.identifier;
                        if multi_assign.contains(&id) {
                            // Already seen twice — skip
                        } else if single_assign_with_ssa.contains_key(&id) {
                            // Second assignment: can't constant-fold, remove both entries
                            single_assign_with_ssa.remove(&id);
                            single_assign_consts.remove(&id);
                            multi_assign.insert(id);
                        } else if let Some(val) = ssa_constants.get(&value.identifier).cloned() {
                            single_assign_consts.insert(id, val.clone());
                            // Track SSA lvalue id paired with orig id so we can remove it
                            // if a second assignment is found.
                            single_assign_with_ssa.insert(id, (instr.lvalue.identifier, val));
                        }
                        // Non-constant assignment: just don't insert
                    }
                }
            }
        }
        for (id, val) in single_assign_consts {
            ssa_constants.insert(id, val);
        }
        // Also add SSA lvalue ids so the rewrite pass can fold LoadLocals that were
        // rewritten to the StoreLocal's instruction lvalue (after phi elimination).
        for (_orig_id, (ssa_id, val)) in single_assign_with_ssa {
            ssa_constants.insert(ssa_id, val);
        }
    }

    // Rewrite pass: substitute constants into instructions.
    // Uses ssa_constants (cross-block) + local_constants (per-block, for let vars).
    // For blocks where x_orig is also mutated (StoreLocal Reassign/Let), we skip ssa_constants
    // for that identifier and rely on local_constants (which respects sequential ordering).
    // For blocks where x_orig is only read, ssa_constants provides the cross-block value.
    let empty_muts: HashSet<IdentifierId> = HashSet::new();
    for block in hir.body.blocks.values_mut() {
        let block_muts = block_mutable_ids.get(&block.id).unwrap_or(&empty_muts);
        let mut local_constants: HashMap<IdentifierId, PrimitiveValue> = HashMap::new();

        for instr in &mut block.instructions {
            // Replace LoadLocal with Primitive if the source is a known constant.
            if let InstructionValue::LoadLocal { place, loc } = &instr.value {
                // For mutable vars in this block, skip ssa_constants to preserve ordering.
                // Also skip ssa_constants for cross-block StoreLocal lvalues: in the React
                // compiler's SSA, loop-body LoadLocals may use the init def's SSA id
                // (e.g., i_init_ssa) as their place instead of the phi result. Allowing
                // ssa_constants to replace those would fold loop variables to their init
                // constant value in all iterations, not just the first.
                let val = if block_muts.contains(&place.identifier)
                    || cross_block_store_lvalues.contains(&place.identifier)
                {
                    local_constants.get(&place.identifier).cloned()
                } else {
                    ssa_constants.get(&place.identifier)
                        .or_else(|| local_constants.get(&place.identifier))
                        .cloned()
                };
                if let Some(val) = val {
                    let loc = loc.clone();
                    instr.value = InstructionValue::Primitive { value: val.clone(), loc };
                    ssa_constants.insert(instr.lvalue.identifier, val);
                }
            }

            // Record any Primitive lvalue as a constant (including newly created ones).
            if let InstructionValue::Primitive { value, .. } = &instr.value {
                ssa_constants.insert(instr.lvalue.identifier, value.clone());
            }

            // Track non-const StoreLocals as block-local constants.
            // Skip closure-captured vars — they may be mutated by closures called later.
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                if lvalue.kind != InstructionKind::Const && lvalue.kind != InstructionKind::HoistedConst
                    && !closure_captured_ids.contains(&lvalue.place.identifier)
                {
                    let val = ssa_constants.get(&value.identifier)
                        .or_else(|| local_constants.get(&value.identifier))
                        .cloned();
                    if let Some(val) = val {
                        local_constants.insert(lvalue.place.identifier, val);
                    } else {
                        local_constants.remove(&lvalue.place.identifier);
                    }
                }
            }

            // Fold UnaryExpression.
            if let InstructionValue::UnaryExpression { value, operator, loc } = &instr.value {
                let val = ssa_constants.get(&value.identifier)
                    .or_else(|| local_constants.get(&value.identifier))
                    .cloned();
                if let Some(val) = val {
                    let loc = loc.clone();
                    if let Some(result) = fold_unary(val, operator) {
                        instr.value = InstructionValue::Primitive { value: result.clone(), loc };
                        ssa_constants.insert(instr.lvalue.identifier, result);
                    }
                }
            }

            // Fold BinaryExpression.
            if let InstructionValue::BinaryExpression { left, right, operator, loc } = &instr.value {
                let lv = ssa_constants.get(&left.identifier)
                    .or_else(|| local_constants.get(&left.identifier))
                    .cloned();
                let rv = ssa_constants.get(&right.identifier)
                    .or_else(|| local_constants.get(&right.identifier))
                    .cloned();
                if let (Some(lv), Some(rv)) = (lv, rv) {
                    let loc = loc.clone();
                    if let Some(result) = fold_binary(lv, rv, operator.clone()) {
                        instr.value = InstructionValue::Primitive { value: result.clone(), loc };
                        ssa_constants.insert(instr.lvalue.identifier, result);
                    }
                }
            }
        }
    }

    // Branch folding: replace If/Branch terminals with Goto when test is constant.
    let mut branches_folded = false;
    for block in hir.body.blocks.values_mut() {
        let should_fold = match &block.terminal {
            // Only fold If terminals. Branch is used for loop tests and
            // ternaries — folding those corrupts loop/for codegen structure.
            Terminal::If { test, consequent, alternate, id, loc, .. } => {
                // Check lattice first (from the main lattice analysis), then fall back to
                // ssa_constants (updated by the rewrite pass above). The rewrite pass may
                // have folded instructions like BinaryExpression after the main lattice
                // converged (e.g., when single-assign Let constants propagated through
                // a comparison), making the folded result available in ssa_constants.
                let const_val = lattice.get(&test.identifier)
                    .and_then(|v| if let LatticeValue::Constant(c) = v { Some(c.clone()) } else { None })
                    .or_else(|| ssa_constants.get(&test.identifier).cloned());
                if let Some(val) = const_val {
                    let target = if is_truthy(&val) { *consequent } else { *alternate };
                    Some(Terminal::Goto {
                        block: target,
                        variant: GotoVariant::Break,
                        id: *id,
                        loc: loc.clone(),
                    })
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(new_terminal) = should_fold {
            block.terminal = new_terminal;
            branches_folded = true;
        }
    }

    branches_folded
}

/// Remove blocks not reachable from the entry block.
fn cp_remove_unreachable_blocks(hir: &mut HIRFunction) {
    let mut reachable: HashSet<BlockId> = HashSet::new();
    let mut queue: Vec<BlockId> = vec![hir.body.entry];
    while let Some(block_id) = queue.pop() {
        if !reachable.insert(block_id) {
            continue;
        }
        if let Some(block) = hir.body.blocks.get(&block_id) {
            for succ in block.terminal.successors() {
                if !reachable.contains(&succ) {
                    queue.push(succ);
                }
            }
        }
    }
    hir.body.blocks.retain(|id, _| reachable.contains(id));
}

/// Remove phi operands whose predecessor block no longer exists.
fn cp_prune_dead_phi_operands(hir: &mut HIRFunction) {
    let existing: HashSet<BlockId> = hir.body.blocks.keys().cloned().collect();
    for block in hir.body.blocks.values_mut() {
        for phi in &mut block.phis {
            phi.operands.retain(|pred_id, _| existing.contains(pred_id));
        }
    }
}

fn fold_unary(val: PrimitiveValue, op: &UnaryOperator) -> Option<PrimitiveValue> {
    match op {
        UnaryOperator::Not => {
            let b = match &val {
                PrimitiveValue::Boolean(b) => !b,
                PrimitiveValue::Number(n) => *n == 0.0 || n.is_nan(),
                PrimitiveValue::String(s) => s.is_empty(),
                PrimitiveValue::Null | PrimitiveValue::Undefined => true,
            };
            Some(PrimitiveValue::Boolean(b))
        }
        UnaryOperator::Minus => match val {
            PrimitiveValue::Number(n) => Some(PrimitiveValue::Number(-n)),
            _ => None,
        },
        UnaryOperator::Plus => match val {
            PrimitiveValue::Number(n) => Some(PrimitiveValue::Number(n)),
            PrimitiveValue::Boolean(b) => Some(PrimitiveValue::Number(if b { 1.0 } else { 0.0 })),
            _ => None,
        },
        UnaryOperator::BitNot => match val {
            PrimitiveValue::Number(n) => Some(PrimitiveValue::Number(!(n as i32) as f64)),
            _ => None,
        },
        UnaryOperator::Typeof => {
            let t = match &val {
                PrimitiveValue::Number(_) => "number",
                PrimitiveValue::Boolean(_) => "boolean",
                PrimitiveValue::String(_) => "string",
                PrimitiveValue::Null => "object",
                PrimitiveValue::Undefined => "undefined",
            };
            Some(PrimitiveValue::String(t.to_string()))
        }
        UnaryOperator::Void => Some(PrimitiveValue::Undefined),
    }
}

fn fold_binary(left: PrimitiveValue, right: PrimitiveValue, op: BinaryOperator) -> Option<PrimitiveValue> {
    // Strict equality/inequality works across all primitive types
    match op {
        BinaryOperator::StrictEq => return Some(PrimitiveValue::Boolean(left == right)),
        BinaryOperator::StrictNEq => return Some(PrimitiveValue::Boolean(left != right)),
        _ => {}
    }

    match (left, right) {
        (PrimitiveValue::String(a), PrimitiveValue::String(b)) => {
            match op {
                BinaryOperator::Add => Some(PrimitiveValue::String(a + &b)),
                BinaryOperator::Lt => Some(PrimitiveValue::Boolean(a < b)),
                BinaryOperator::LtEq => Some(PrimitiveValue::Boolean(a <= b)),
                BinaryOperator::Gt => Some(PrimitiveValue::Boolean(a > b)),
                BinaryOperator::GtEq => Some(PrimitiveValue::Boolean(a >= b)),
                _ => None,
            }
        }
        (PrimitiveValue::Number(a), PrimitiveValue::Number(b)) => {
            // Comparison operators return boolean
            match op {
                BinaryOperator::Lt => return Some(PrimitiveValue::Boolean(a < b)),
                BinaryOperator::LtEq => return Some(PrimitiveValue::Boolean(a <= b)),
                BinaryOperator::Gt => return Some(PrimitiveValue::Boolean(a > b)),
                BinaryOperator::GtEq => return Some(PrimitiveValue::Boolean(a >= b)),
                BinaryOperator::Eq => return Some(PrimitiveValue::Boolean(a == b)),
                BinaryOperator::NEq => return Some(PrimitiveValue::Boolean(a != b)),
                _ => {}
            }
            let result = match op {
                BinaryOperator::Add => a + b,
                BinaryOperator::Sub => a - b,
                BinaryOperator::Mul => a * b,
                BinaryOperator::Div if b != 0.0 => a / b,
                BinaryOperator::Mod if b != 0.0 => a % b,
                BinaryOperator::Exp => a.powf(b),
                // Bitwise (convert to i32/u32)
                BinaryOperator::BitAnd => ((a as i32) & (b as i32)) as f64,
                BinaryOperator::BitOr  => ((a as i32) | (b as i32)) as f64,
                BinaryOperator::BitXor => ((a as i32) ^ (b as i32)) as f64,
                BinaryOperator::Shl    => ((a as i32).wrapping_shl(b as u32 & 31)) as f64,
                BinaryOperator::Shr    => ((a as i32).wrapping_shr(b as u32 & 31)) as f64,
                BinaryOperator::UShr   => ((a as u32).wrapping_shr(b as u32 & 31)) as f64,
                _ => return None,
            };
            Some(PrimitiveValue::Number(result))
        }
        _ => None,
    }
}
