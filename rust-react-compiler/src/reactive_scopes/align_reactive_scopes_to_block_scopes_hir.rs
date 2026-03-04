/// Align reactive scope ranges to block boundaries.
///
/// Port of AlignReactiveScopesToBlockScopesHIR.ts.
///
/// After scope inference, scope ranges are defined by instruction IDs at arbitrary
/// points in the CFG. But to codegen blocks around the instructions in each scope,
/// the scopes must be aligned to block-scope boundaries — we can't memoize half
/// of a loop or half of an if-branch.
///
/// This pass walks blocks in order, tracking active scopes and extending their
/// ranges when they cross block boundaries (fallthroughs, value blocks, labeled
/// breaks).
use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{
    BlockId, BlockKind, HIRFunction, IdentifierId, InstructionId, MutableRange, ScopeId, Terminal,
};
use crate::hir::visitors::{each_instruction_value_operand, each_terminal_operand};

pub fn run(hir: &mut HIRFunction) {
    run_with_env(hir, None);
}

pub fn run_with_env(hir: &mut HIRFunction, env: Option<&mut Environment>) {
    let env = match env {
        Some(e) => e,
        None => return,
    };

    // Pre-build identifier -> scope_id map (read-only, doesn't change during pass)
    let ident_scope: HashMap<IdentifierId, ScopeId> = env
        .identifiers
        .iter()
        .filter_map(|(id, ident)| ident.scope.map(|s| (*id, s)))
        .collect();

    // Clone scope ranges for in-place modification; written back at end
    let mut scope_ranges: HashMap<ScopeId, MutableRange> = env
        .scopes
        .iter()
        .map(|(id, s)| (*id, s.range.clone()))
        .collect();

    // Stack of block-fallthrough ranges: (range, fallthrough_block)
    let mut active_block_fallthrough_ranges: Vec<(MutableRange, BlockId)> = Vec::new();
    // Currently active scopes (their range overlaps current position)
    let mut active_scopes: HashSet<ScopeId> = HashSet::new();
    // Scopes already seen (for first-encounter value range extension)
    let mut seen: HashSet<ScopeId> = HashSet::new();
    // Value range for blocks inside value blocks (ternary/logical/optional)
    let mut value_block_ranges: HashMap<BlockId, MutableRange> = HashMap::new();

    let block_ids: Vec<BlockId> = hir.body.blocks.keys().copied().collect();

    for &bid in &block_ids {
        let block = &hir.body.blocks[&bid];
        let starting_id = block
            .instructions
            .first()
            .map(|i| i.id)
            .unwrap_or(block.terminal.id());

        // Prune scopes that have ended
        active_scopes.retain(|scope_id| {
            scope_ranges
                .get(scope_id)
                .map(|r| r.end > starting_id)
                .unwrap_or(false)
        });

        // Check if current block is a fallthrough target from the stack.
        // If so, extend all active scopes' start backward to cover the block range.
        if let Some((range, ft)) = active_block_fallthrough_ranges.last() {
            if *ft == bid {
                let range_start = range.start;
                active_block_fallthrough_ranges.pop();
                for &scope_id in &active_scopes {
                    if let Some(r) = scope_ranges.get_mut(&scope_id) {
                        if range_start < r.start {
                            r.start = range_start;
                        }
                    }
                }
            }
        }

        // Get value range for this block (if inside a value block)
        let value_range = value_block_ranges.get(&bid).cloned();

        // Collect all places from instructions and terminal operands
        let block = &hir.body.blocks[&bid];
        let mut places: Vec<(InstructionId, IdentifierId)> = Vec::new();
        for instr in &block.instructions {
            // lvalue
            places.push((instr.id, instr.lvalue.identifier));
            // operands
            for op in each_instruction_value_operand(&instr.value) {
                places.push((instr.id, op.identifier));
            }
        }
        for op in each_terminal_operand(&block.terminal) {
            places.push((block.terminal.id(), op.identifier));
        }

        // Process places (recordPlace logic from TS)
        for (instr_id, ident_id) in places {
            let scope_id = match ident_scope.get(&ident_id) {
                Some(s) => *s,
                None => continue,
            };
            // getPlaceScope: check if instruction is within scope's range
            let in_range = scope_ranges
                .get(&scope_id)
                .map(|r| instr_id >= r.start && instr_id < r.end)
                .unwrap_or(false);
            if !in_range {
                continue;
            }
            active_scopes.insert(scope_id);
            if seen.insert(scope_id) {
                // First encounter — extend to value range if inside value block
                if let Some(vr) = &value_range {
                    if let Some(r) = scope_ranges.get_mut(&scope_id) {
                        if vr.start < r.start {
                            r.start = vr.start;
                        }
                        if vr.end > r.end {
                            r.end = vr.end;
                        }
                    }
                }
            }
        }

        // Get terminal info
        let block = &hir.body.blocks[&bid];
        let terminal = &block.terminal;
        let terminal_id = terminal.id();
        // The TS terminalFallthrough() returns null for loop terminals,
        // goto, return, throw, unreachable. We must match that behavior.
        let fallthrough = match terminal {
            Terminal::DoWhile { .. }
            | Terminal::While { .. }
            | Terminal::For { .. }
            | Terminal::ForOf { .. }
            | Terminal::ForIn { .. } => None,
            _ => terminal.fallthrough(),
        };
        let is_branch = matches!(terminal, Terminal::Branch { .. });

        // Handle non-branch fallthrough: extend active scopes and push to stack
        if let Some(ft) = fallthrough {
            if !is_branch {
                let ft_block = match hir.body.blocks.get(&ft) {
                    Some(b) => b,
                    None => continue,
                };
                let next_id = ft_block
                    .instructions
                    .first()
                    .map(|i| i.id)
                    .unwrap_or(ft_block.terminal.id());

                // Extend active scopes that overlap beyond the terminal
                for &scope_id in &active_scopes {
                    if let Some(r) = scope_ranges.get_mut(&scope_id) {
                        if r.end > terminal_id && next_id > r.end {
                            r.end = next_id;
                        }
                    }
                }

                // Push to block-fallthrough stack
                active_block_fallthrough_ranges.push((
                    MutableRange {
                        start: terminal_id,
                        end: next_id,
                    },
                    ft,
                ));

                // Propagate value range to fallthrough (if in value block)
                if let Some(vr) = &value_range {
                    value_block_ranges.entry(ft).or_insert_with(|| vr.clone());
                }
            }
        }

        // Handle goto-to-label: extend scopes to cover the labeled range
        // so a break doesn't accidentally jump out of a scope
        if let Terminal::Goto {
            block: goto_target, ..
        } = terminal
        {
            if let Some(pos) = active_block_fallthrough_ranges
                .iter()
                .position(|(_, ft)| ft == goto_target)
            {
                // Only handle if it's NOT the topmost entry (topmost = natural fallthrough)
                if pos < active_block_fallthrough_ranges.len().saturating_sub(1) {
                    let start_range_start = active_block_fallthrough_ranges[pos].0.start;
                    let start_ft = active_block_fallthrough_ranges[pos].1;
                    let ft_block = match hir.body.blocks.get(&start_ft) {
                        Some(b) => b,
                        None => continue,
                    };
                    let first_id = ft_block
                        .instructions
                        .first()
                        .map(|i| i.id)
                        .unwrap_or(ft_block.terminal.id());

                    for &scope_id in &active_scopes {
                        if let Some(r) = scope_ranges.get_mut(&scope_id) {
                            if r.end <= terminal_id {
                                continue;
                            }
                            if start_range_start < r.start {
                                r.start = start_range_start;
                            }
                            if first_id > r.end {
                                r.end = first_id;
                            }
                        }
                    }
                }
            }
        }

        // Propagate value block ranges to successors
        let terminal = &hir.body.blocks[&bid].terminal;
        let is_ternary_logical_optional = matches!(
            terminal,
            Terminal::Ternary { .. } | Terminal::Logical { .. } | Terminal::Optional { .. }
        );
        let successors = terminal.successors();

        for succ in successors {
            if value_block_ranges.contains_key(&succ) {
                continue;
            }
            let succ_block = match hir.body.blocks.get(&succ) {
                Some(b) => b,
                None => continue,
            };

            if succ_block.kind == BlockKind::Block || succ_block.kind == BlockKind::Catch {
                // Block/catch successors don't get value ranges
            } else if value_range.is_none() || is_ternary_logical_optional {
                // Create new value range (block->value transition or ternary/logical/optional)
                let vr = if value_range.is_none() {
                    // block -> value transition: derive range from terminal to fallthrough
                    if let Some(ft) = fallthrough {
                        let ft_block = match hir.body.blocks.get(&ft) {
                            Some(b) => b,
                            None => continue,
                        };
                        let next_id = ft_block
                            .instructions
                            .first()
                            .map(|i| i.id)
                            .unwrap_or(ft_block.terminal.id());
                        MutableRange {
                            start: terminal_id,
                            end: next_id,
                        }
                    } else {
                        continue;
                    }
                } else {
                    // value -> value with ternary/logical/optional: reuse parent range
                    value_range.as_ref().unwrap().clone()
                };
                value_block_ranges.insert(succ, vr);
            } else {
                // value -> value reuse (non-ternary/logical/optional)
                if let Some(vr) = &value_range {
                    value_block_ranges.insert(succ, vr.clone());
                }
            }
        }
    }

    // Loop-entry alignment: if a scope starts mid-block in a block with a loop
    // terminal, expand its start to the first instruction of that block.
    // This ensures all instructions in the block are inside or outside the scope.
    {
        let mut instr_to_block: HashMap<InstructionId, BlockId> = HashMap::new();
        let mut block_first_instr: HashMap<BlockId, InstructionId> = HashMap::new();
        for (&bid, block) in &hir.body.blocks {
            if let Some(first) = block.instructions.first() {
                block_first_instr.insert(bid, first.id);
                for instr in &block.instructions {
                    instr_to_block.insert(instr.id, bid);
                }
            }
        }
        for (scope_id, range) in scope_ranges.iter_mut() {
            if let Some(&containing_block) = instr_to_block.get(&range.start) {
                let is_loop_entry = matches!(
                    hir.body.blocks.get(&containing_block).map(|b| &b.terminal),
                    Some(Terminal::DoWhile { .. })
                    | Some(Terminal::While { .. })
                    | Some(Terminal::For { .. })
                    | Some(Terminal::ForOf { .. })
                    | Some(Terminal::ForIn { .. })
                );
                if is_loop_entry {
                    if let Some(&first_instr) = block_first_instr.get(&containing_block) {
                        if first_instr < range.start {
                            range.start = first_instr;
                        }
                    }
                }
            }
        }
    }

    // Write back modified ranges to env
    for (scope_id, range) in &scope_ranges {
        if let Some(scope) = env.scopes.get_mut(scope_id) {
            scope.range = range.clone();
        }
    }
}
