/// Align reactive scope ranges to block boundaries.
///
/// This is a simplified port of AlignReactiveScopesToBlockScopes.ts.
///
/// Core problem: after scope inference, scope ranges are defined by instruction IDs.
/// But the block structure means that when a scope spans across a loop boundary,
/// the "pre-loop" instructions in the entry block (the block with the loop terminal)
/// may be OUTSIDE the scope's range even though they logically belong inside the
/// memoized block.
///
/// Example: `do-while-simple.js`
///   BlockId(0): instrs 0-6 (entry block with DoWhile terminal)
///   BlockId(1): instrs 8-17 (loop body)
///   Scope range: [5, 24] — starts at instr 5 (in BlockId(0))
///   BUT instrs 0-4 in BlockId(0) are outside the scope.
///
/// Solution: when a scope starts somewhere in the middle of a block, expand its
/// range start to cover the first instruction of that block (so all instructions
/// in the block are either in or out of the scope, not split).
///
/// This ensures the codegen's loop-wrapping logic correctly includes all
/// pre-loop instructions inside the memoized block.
use std::collections::HashMap;

use crate::hir::environment::Environment;
use crate::hir::hir::{BlockId, HIRFunction, InstructionId, ScopeId, Terminal};

pub fn run(hir: &mut HIRFunction) {
    run_with_env(hir, None);
}

pub fn run_with_env(hir: &mut HIRFunction, env: Option<&mut Environment>) {
    // For each block, collect the range of instruction IDs it contains.
    // block_instr_range: BlockId -> (first_instr_id, last_instr_id)
    let mut block_instr_range: HashMap<BlockId, (InstructionId, InstructionId)> = HashMap::new();
    for (&bid, block) in &hir.body.blocks {
        if let (Some(first), Some(last)) = (
            block.instructions.first().map(|i| i.id),
            block.instructions.last().map(|i| i.id),
        ) {
            block_instr_range.insert(bid, (first, last));
        }
    }

    // Build instr_to_block map.
    let mut instr_to_block: HashMap<InstructionId, BlockId> = HashMap::new();
    for (&bid, block) in &hir.body.blocks {
        for instr in &block.instructions {
            instr_to_block.insert(instr.id, bid);
        }
    }

    // Identify loop body blocks: blocks whose sole predecessor has a loop terminal
    // pointing to this block as the loop body.
    // For each loop terminal block, collect the set of blocks that are "inside" the loop.
    let mut loop_entry_blocks: HashMap<BlockId, BlockId> = HashMap::new(); // loop_body_bid -> entry_bid
    for (&bid, block) in &hir.body.blocks {
        match &block.terminal {
            Terminal::DoWhile { loop_, .. } => {
                loop_entry_blocks.insert(*loop_, bid);
            }
            Terminal::While { loop_, .. } => {
                loop_entry_blocks.insert(*loop_, bid);
            }
            Terminal::For { loop_, .. } => {
                loop_entry_blocks.insert(*loop_, bid);
            }
            Terminal::ForOf { loop_, .. } => {
                loop_entry_blocks.insert(*loop_, bid);
            }
            Terminal::ForIn { loop_, .. } => {
                loop_entry_blocks.insert(*loop_, bid);
            }
            _ => {}
        }
    }

    // For each scope in env (if provided), check if the scope has instructions
    // that start in the MIDDLE of a block that is a loop entry block.
    // If so, expand the scope's range start to include the first instruction of that block.
    if let Some(env) = env {
        let scope_ids: Vec<ScopeId> = env.scopes.keys().copied().collect();
        for sid in scope_ids {
            let scope_range = if let Some(s) = env.scopes.get(&sid) {
                s.range.clone()
            } else {
                continue;
            };

            // Find the block that contains the scope's first instruction.
            // We look for any block where the first scope instruction falls mid-block.
            let scope_start_instr = scope_range.start;

            // Find the block containing scope_start_instr.
            // The scope's start instruction should be in some block.
            let containing_block = instr_to_block.get(&scope_start_instr).copied();

            if let Some(cblock) = containing_block {
                // Check if this block is a loop entry block (it has a loop terminal).
                let is_loop_entry = matches!(
                    hir.body.blocks.get(&cblock).map(|b| &b.terminal),
                    Some(Terminal::DoWhile { .. })
                    | Some(Terminal::While { .. })
                    | Some(Terminal::For { .. })
                    | Some(Terminal::ForOf { .. })
                    | Some(Terminal::ForIn { .. })
                );
                if is_loop_entry {
                    // Expand the scope's range to include the first instruction of this block.
                    if let Some((first_instr, _)) = block_instr_range.get(&cblock) {
                        if *first_instr < scope_start_instr {
                            if let Some(scope) = env.scopes.get_mut(&sid) {
                                if std::env::var("RC_DEBUG").is_ok() {
                                    eprintln!("[align_scopes] scope {:?} range [{:?},{:?}] → expanding start to {:?} (block {:?} is loop entry)",
                                        sid.0, scope.range.start.0, scope.range.end.0, first_instr.0, cblock);
                                }
                                scope.range.start = *first_instr;
                            }
                        }
                    }
                }
            }
        }
    }
}
