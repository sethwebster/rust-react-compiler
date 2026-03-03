use crate::hir::environment::Environment;
use crate::hir::hir::{BlockId, HIRFunction, Terminal};

pub fn run(hir: &mut HIRFunction) {
    let mut active_loops: Vec<BlockId> = Vec::new();
    let block_ids: Vec<BlockId> = hir.body.blocks.keys().copied().collect();

    for block_id in block_ids {
        active_loops.retain(|id| *id != block_id);

        let Some(block) = hir.body.blocks.get_mut(&block_id) else {
            continue;
        };
        let terminal = block.terminal.clone();

        match terminal {
            Terminal::DoWhile { fallthrough, .. }
            | Terminal::For { fallthrough, .. }
            | Terminal::ForIn { fallthrough, .. }
            | Terminal::ForOf { fallthrough, .. }
            | Terminal::While { fallthrough, .. } => {
                active_loops.push(fallthrough);
            }
            Terminal::ReactiveScope {
                scope,
                block: scope_block,
                fallthrough,
                id,
                loc,
            } => {
                if !active_loops.is_empty() {
                    block.terminal = Terminal::PrunedScope {
                        scope,
                        block: scope_block,
                        fallthrough,
                        id,
                        loc,
                    };
                }
            }
            _ => {}
        }
    }
}

pub fn run_with_env(hir: &mut HIRFunction, _env: &mut Environment) {
    if std::env::var("RC_ENABLE_FLATTEN_REACTIVE_LOOPS").is_err() {
        return;
    }
    run(hir);
}
