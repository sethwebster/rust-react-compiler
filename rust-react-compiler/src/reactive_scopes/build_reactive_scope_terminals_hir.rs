use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::hir::environment::Environment;
use crate::hir::hir::{
    BasicBlock, BlockId, GotoVariant, HIRFunction, InstructionId, ReactiveScope, ScopeId,
    SourceLocation, Terminal,
};

#[derive(Clone)]
enum TerminalRewriteInfo {
    StartScope {
        block_id: BlockId,
        fallthrough_id: BlockId,
        instr_id: InstructionId,
        scope: ReactiveScope,
    },
    EndScope {
        instr_id: InstructionId,
        fallthrough_id: BlockId,
    },
}

impl TerminalRewriteInfo {
    fn instr_id(&self) -> InstructionId {
        match self {
            TerminalRewriteInfo::StartScope { instr_id, .. }
            | TerminalRewriteInfo::EndScope { instr_id, .. } => *instr_id,
        }
    }
}

struct RewriteContext {
    source: BasicBlock,
    instr_slice_idx: usize,
    next_preds: HashSet<BlockId>,
    next_block_id: BlockId,
    rewrites: Vec<BasicBlock>,
}

pub fn run(_hir: &mut HIRFunction) {}

pub fn run_with_env(hir: &mut HIRFunction, env: &mut Environment) {
    if std::env::var("RC_ENABLE_SCOPE_TERMINALS_HIR").is_err() {
        return;
    }

    if env.scopes.is_empty() {
        return;
    }

    let mut scopes: Vec<ReactiveScope> = env
        .scopes
        .values()
        .filter(|scope| scope.range.start != scope.range.end)
        .cloned()
        .collect();
    if scopes.is_empty() {
        return;
    }

    let mut queued_rewrites: Vec<TerminalRewriteInfo> = Vec::new();
    let mut fallthroughs: HashMap<ScopeId, BlockId> = HashMap::new();

    if !collect_scope_rewrites(&mut scopes, env, &mut fallthroughs, &mut queued_rewrites) {
        // Invalid scope nesting: fail safe and leave CFG unchanged.
        return;
    }
    if queued_rewrites.is_empty() {
        return;
    }

    // Reversed so we can pop from the end while walking instructions ascending.
    queued_rewrites.reverse();

    let original_blocks = std::mem::take(&mut hir.body.blocks);
    let mut rewritten_final_blocks: HashMap<BlockId, BlockId> = HashMap::new();
    let mut next_blocks: IndexMap<BlockId, BasicBlock> = IndexMap::new();

    for (_, block) in &original_blocks {
        let mut context = RewriteContext {
            next_block_id: block.id,
            rewrites: Vec::new(),
            next_preds: block.preds.clone(),
            instr_slice_idx: 0,
            source: block.clone(),
        };

        for i in 0..=block.instructions.len() {
            let instr_id = if i < block.instructions.len() {
                block.instructions[i].id
            } else {
                block.terminal.id()
            };

            loop {
                let should_apply = match queued_rewrites.last() {
                    Some(rewrite) => rewrite.instr_id() <= instr_id,
                    None => false,
                };
                if !should_apply {
                    break;
                }
                let rewrite = queued_rewrites.pop().expect("checked is_some");
                handle_rewrite(rewrite, i, &mut context);
            }
        }

        if context.rewrites.is_empty() {
            next_blocks.insert(block.id, block.clone());
            continue;
        }

        let final_block = BasicBlock {
            id: context.next_block_id,
            kind: block.kind,
            preds: context.next_preds,
            terminal: block.terminal.clone(),
            instructions: block.instructions[context.instr_slice_idx..].to_vec(),
            phis: Vec::new(),
        };
        context.rewrites.push(final_block.clone());

        for rewritten in context.rewrites {
            next_blocks.insert(rewritten.id, rewritten);
        }
        rewritten_final_blocks.insert(block.id, final_block.id);
    }

    // Repoint phi operands that referenced a rewritten predecessor block.
    for (_, block) in &mut next_blocks {
        for phi in &mut block.phis {
            let keys: Vec<BlockId> = phi.operands.keys().copied().collect();
            for old_id in keys {
                if let Some(&new_id) = rewritten_final_blocks.get(&old_id) {
                    if old_id != new_id {
                        if let Some(place) = phi.operands.remove(&old_id) {
                            phi.operands.insert(new_id, place);
                        }
                    }
                }
            }
        }
    }

    hir.body.blocks = next_blocks;
    let entry = hir.body.entry;
    reorder_blocks_rpo(&mut hir.body, entry);
    fix_predecessors(&mut hir.body);
}

fn range_preorder_comparator(a: &ReactiveScope, b: &ReactiveScope) -> Ordering {
    let start = a.range.start.cmp(&b.range.start);
    if start != Ordering::Equal {
        return start;
    }
    b.range.end.cmp(&a.range.end)
}

fn collect_scope_rewrites(
    scopes: &mut [ReactiveScope],
    env: &mut Environment,
    fallthroughs: &mut HashMap<ScopeId, BlockId>,
    rewrites: &mut Vec<TerminalRewriteInfo>,
) -> bool {
    scopes.sort_by(range_preorder_comparator);
    let mut active: Vec<ReactiveScope> = Vec::new();

    for scope in scopes.iter() {
        loop {
            let Some(parent) = active.last() else {
                break;
            };
            let disjoint = scope.range.start >= parent.range.end;
            let nested = scope.range.end <= parent.range.end;
            if !disjoint && !nested {
                return false;
            }
            if disjoint {
                let finished = active.pop().expect("active not empty");
                push_end_scope_rewrite(&finished, fallthroughs, rewrites);
            } else {
                break;
            }
        }

        push_start_scope_rewrite(scope, env, fallthroughs, rewrites);
        active.push(scope.clone());
    }

    while let Some(scope) = active.pop() {
        push_end_scope_rewrite(&scope, fallthroughs, rewrites);
    }

    true
}

fn push_start_scope_rewrite(
    scope: &ReactiveScope,
    env: &mut Environment,
    fallthroughs: &mut HashMap<ScopeId, BlockId>,
    rewrites: &mut Vec<TerminalRewriteInfo>,
) {
    let block_id = env.new_block_id();
    let fallthrough_id = env.new_block_id();
    rewrites.push(TerminalRewriteInfo::StartScope {
        block_id,
        fallthrough_id,
        instr_id: scope.range.start,
        scope: scope.clone(),
    });
    fallthroughs.insert(scope.id, fallthrough_id);
}

fn push_end_scope_rewrite(
    scope: &ReactiveScope,
    fallthroughs: &HashMap<ScopeId, BlockId>,
    rewrites: &mut Vec<TerminalRewriteInfo>,
) {
    let Some(&fallthrough_id) = fallthroughs.get(&scope.id) else {
        return;
    };
    rewrites.push(TerminalRewriteInfo::EndScope {
        instr_id: scope.range.end,
        fallthrough_id,
    });
}

fn handle_rewrite(terminal_info: TerminalRewriteInfo, idx: usize, context: &mut RewriteContext) {
    let terminal = match &terminal_info {
        TerminalRewriteInfo::StartScope {
            block_id,
            fallthrough_id,
            instr_id,
            scope,
        } => Terminal::ReactiveScope {
            scope: scope.clone(),
            block: *block_id,
            fallthrough: *fallthrough_id,
            id: *instr_id,
            loc: SourceLocation::Generated,
        },
        TerminalRewriteInfo::EndScope {
            instr_id,
            fallthrough_id,
        } => Terminal::Goto {
            block: *fallthrough_id,
            variant: GotoVariant::Try,
            id: *instr_id,
            loc: SourceLocation::Generated,
        },
    };

    let curr_block_id = context.next_block_id;
    context.rewrites.push(BasicBlock {
        kind: context.source.kind,
        id: curr_block_id,
        instructions: context.source.instructions[context.instr_slice_idx..idx].to_vec(),
        preds: context.next_preds.clone(),
        phis: if context.rewrites.is_empty() {
            context.source.phis.clone()
        } else {
            Vec::new()
        },
        terminal,
    });

    context.next_preds = HashSet::from([curr_block_id]);
    context.next_block_id = match terminal_info {
        TerminalRewriteInfo::StartScope { block_id, .. } => block_id,
        TerminalRewriteInfo::EndScope { fallthrough_id, .. } => fallthrough_id,
    };
    context.instr_slice_idx = idx;
}

fn fix_predecessors(hir: &mut crate::hir::hir::HIR) {
    let mut preds: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();
    for (&id, block) in &hir.blocks {
        for succ in block.terminal.successors() {
            preds.entry(succ).or_default().insert(id);
        }
    }
    for (&id, block) in &mut hir.blocks {
        block.preds = preds.remove(&id).unwrap_or_default();
    }
}

fn reorder_blocks_rpo(hir: &mut crate::hir::hir::HIR, entry: BlockId) {
    fn dfs(
        bid: BlockId,
        blocks: &IndexMap<BlockId, BasicBlock>,
        visited: &mut HashSet<BlockId>,
        post: &mut Vec<BlockId>,
    ) {
        if !visited.insert(bid) {
            return;
        }
        let Some(block) = blocks.get(&bid) else {
            return;
        };
        for succ in block.terminal.successors() {
            if blocks.contains_key(&succ) {
                dfs(succ, blocks, visited, post);
            }
        }
        post.push(bid);
    }

    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut post: Vec<BlockId> = Vec::new();
    dfs(entry, &hir.blocks, &mut visited, &mut post);
    post.reverse(); // reverse postorder

    let mut ordered_ids: Vec<BlockId> = post;
    for bid in hir.blocks.keys().copied() {
        if !visited.contains(&bid) {
            ordered_ids.push(bid);
        }
    }

    let mut reordered: IndexMap<BlockId, BasicBlock> = IndexMap::new();
    for bid in ordered_ids {
        if let Some(block) = hir.blocks.get(&bid) {
            reordered.insert(bid, block.clone());
        }
    }
    hir.blocks = reordered;
}
