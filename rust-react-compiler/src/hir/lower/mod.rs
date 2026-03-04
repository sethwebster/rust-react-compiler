/// HIR lowering infrastructure — Rust port of HIRBuilder.ts
///
/// Agents implement specific lowering functions in submodules:
///   core.rs        — entry point, statement dispatch
///   expressions.rs — literals, identifiers, binary, unary
///   calls.rs       — CallExpression, MethodCall, NewExpression
///   properties.rs  — PropertyLoad/Store, ComputedLoad/Store
///   control_flow.rs — if/else, ternary, logical, optional, switch
///   loops.rs       — while, for, for-of, for-in, break/continue
///   functions.rs   — FunctionExpression, ArrowFunction (nested HIRFunction)
///   jsx.rs         — JsxElement, JsxFragment
///   patterns.rs    — destructuring ArrayPattern, ObjectPattern

pub mod core;
pub mod expressions;
pub mod calls;
pub mod properties;
pub mod control_flow;
pub mod loops;
pub mod functions;
pub mod jsx;
pub mod patterns;

use indexmap::IndexMap;
use rustc_hash::FxHashMap;

use crate::hir::environment::Environment;
use crate::hir::hir::*;

// ---------------------------------------------------------------------------
// WipBlock — a block being built, without a terminal yet
// ---------------------------------------------------------------------------

pub struct WipBlock {
    pub id: BlockId,
    pub kind: BlockKind,
    pub instructions: Vec<Instruction>,
    pub preds: std::collections::HashSet<BlockId>,
    pub phis: Vec<Phi>,
}

impl WipBlock {
    pub fn new(id: BlockId, kind: BlockKind) -> Self {
        WipBlock {
            id,
            kind,
            instructions: Vec::new(),
            preds: std::collections::HashSet::new(),
            phis: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Scope stack entry — for break/continue/label resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Scope {
    Loop {
        label: Option<String>,
        continue_block: BlockId,
        break_block: BlockId,
    },
    Switch {
        label: Option<String>,
        break_block: BlockId,
    },
    Label {
        label: String,
        break_block: BlockId,
    },
}

// ---------------------------------------------------------------------------
// LoweringContext — central builder state
// ---------------------------------------------------------------------------

pub struct LoweringContext<'env> {
    pub env: &'env mut Environment,

    /// oxc SymbolId (u32) → our IdentifierId
    pub symbol_map: FxHashMap<u32, IdentifierId>,

    /// Completed BasicBlocks (in order of creation)
    pub completed: IndexMap<BlockId, BasicBlock>,

    /// The block currently being built
    pub current: WipBlock,

    /// Entry block id
    pub entry: BlockId,

    /// Scope stack for break/continue/label resolution
    pub scopes: Vec<Scope>,

    /// Exception handler stack (innermost try's catch block)
    pub exception_handler_stack: Vec<BlockId>,

    /// True when the current block is a dead placeholder (after terminate() with no live successor)
    pub current_dead: bool,
}

impl<'env> LoweringContext<'env> {
    pub fn new(env: &'env mut Environment) -> Self {
        let entry_id = env.new_block_id();
        LoweringContext {
            env,
            symbol_map: FxHashMap::default(),
            completed: IndexMap::new(),
            current: WipBlock::new(entry_id, BlockKind::Block),
            entry: entry_id,
            scopes: Vec::new(),
            exception_handler_stack: Vec::new(),
            current_dead: false,
        }
    }

    pub fn is_current_dead(&self) -> bool {
        self.current_dead
    }

    // -----------------------------------------------------------------------
    // Block management
    // -----------------------------------------------------------------------

    /// Reserve a new block ID without switching to it.
    pub fn reserve(&mut self, kind: BlockKind) -> BlockId {
        self.env.new_block_id()
    }

    /// Seal the current block with `terminal`, then start building `next_id`.
    /// `next_id` must have been previously reserved with `reserve()`.
    pub fn terminate_with_fallthrough(&mut self, terminal: Terminal, next_id: BlockId, next_kind: BlockKind) {
        self.seal_current(terminal);
        self.current = WipBlock::new(next_id, next_kind);
        self.current_dead = false;
    }

    /// Seal the current block with `terminal`. Does NOT start a new block.
    /// Call this for terminals with no fallthrough (return, throw, unreachable).
    pub fn terminate(&mut self, terminal: Terminal) {
        self.seal_current(terminal);
        // Start a dead block so push() calls after this don't panic
        let dead_id = self.env.new_block_id();
        self.current = WipBlock::new(dead_id, BlockKind::Block);
        self.current_dead = true;
    }

    fn seal_current(&mut self, terminal: Terminal) {
        let wip = std::mem::replace(
            &mut self.current,
            WipBlock::new(BlockId(u32::MAX), BlockKind::Block),
        );
        // Record predecessor edges
        let block_id = wip.id;
        let succs = terminal.successors();
        let block = BasicBlock {
            kind: wip.kind,
            id: wip.id,
            instructions: wip.instructions,
            terminal,
            preds: wip.preds,
            phis: wip.phis,
        };
        self.completed.insert(block_id, block);
        // Add this block as a predecessor of all its successors
        for succ in succs {
            if let Some(b) = self.completed.get_mut(&succ) {
                b.preds.insert(block_id);
            }
            // If not yet completed, we'll fix up preds during SSA construction
        }
    }

    /// Switch to building a previously reserved block (without sealing current).
    /// The caller must have already terminated the current block.
    pub fn switch_to(&mut self, id: BlockId, kind: BlockKind) {
        self.current = WipBlock::new(id, kind);
        self.current_dead = false;
    }

    // -----------------------------------------------------------------------
    // Instruction emission
    // -----------------------------------------------------------------------

    /// Push an instruction with a fresh temporary lvalue. Returns the lvalue place.
    pub fn push(&mut self, value: InstructionValue, loc: SourceLocation) -> Place {
        let id = self.env.new_instruction_id();
        let tmp_id = self.env.new_temporary(loc.clone());
        let lvalue = Place::new(tmp_id, loc.clone());
        let instr = Instruction {
            id,
            lvalue: lvalue.clone(),
            value,
            loc,
            effects: None,
        };
        self.current.instructions.push(instr);
        lvalue
    }

    /// Push an instruction with a specific lvalue place.
    pub fn push_with_lvalue(&mut self, lvalue: Place, value: InstructionValue, loc: SourceLocation) {
        let id = self.env.new_instruction_id();
        let instr = Instruction {
            id,
            lvalue,
            value,
            loc,
            effects: None,
        };
        self.current.instructions.push(instr);
    }

    // -----------------------------------------------------------------------
    // Identifier resolution
    // -----------------------------------------------------------------------

    /// Create or return the IdentifierId for an oxc symbol.
    pub fn get_or_create_symbol(
        &mut self,
        symbol_id: u32,
        name: Option<&str>,
        loc: SourceLocation,
    ) -> IdentifierId {
        if let Some(&id) = self.symbol_map.get(&symbol_id) {
            return id;
        }
        let id = self.env.new_identifier_id();
        let decl_id = self.env.new_declaration_id();
        let ident = Identifier {
            id,
            declaration_id: decl_id,
            name: name.map(|n| IdentifierName::Named(n.to_string())),
            mutable_range: MutableRange::zero(),
            scope: None,
            type_: Type::default(),
            loc,
        };
        self.env.identifiers.insert(id, ident);
        self.symbol_map.insert(symbol_id, id);
        id
    }

    /// Create a fresh temporary place.
    pub fn make_temporary(&mut self, loc: SourceLocation) -> Place {
        let id = self.env.new_temporary(loc.clone());
        Place::new(id, loc)
    }

    // -----------------------------------------------------------------------
    // Scope management
    // -----------------------------------------------------------------------

    pub fn push_scope(&mut self, scope: Scope) {
        self.scopes.push(scope);
    }

    pub fn pop_scope(&mut self) -> Option<Scope> {
        self.scopes.pop()
    }

    /// Find the break target for a labeled or unlabeled break.
    pub fn find_break_target(&self, label: Option<&str>) -> Option<BlockId> {
        for scope in self.scopes.iter().rev() {
            match scope {
                Scope::Loop { label: l, break_block, .. }
                | Scope::Switch { label: l, break_block } => {
                    if label.is_none() || l.as_deref() == label {
                        return Some(*break_block);
                    }
                }
                Scope::Label { label: l, break_block } => {
                    if label == Some(l.as_str()) {
                        return Some(*break_block);
                    }
                }
            }
        }
        None
    }

    /// Find the continue target for a labeled or unlabeled continue.
    pub fn find_continue_target(&self, label: Option<&str>) -> Option<BlockId> {
        for scope in self.scopes.iter().rev() {
            if let Scope::Loop { label: l, continue_block, .. } = scope {
                if label.is_none() || l.as_deref() == label {
                    return Some(*continue_block);
                }
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // Finalize
    // -----------------------------------------------------------------------

    /// Finalize the HIR. The current block should have been terminated already.
    /// If it hasn't, insert an unreachable terminal.
    pub fn build(mut self, returns: Place) -> (HIR, Place) {
        // Terminate dangling current block as unreachable
        let dead_id = self.current.id;
        if dead_id.0 != u32::MAX {
            let id = self.env.new_instruction_id();
            let loc = SourceLocation::Generated;
            self.seal_current(Terminal::Unreachable { id, loc });
        }
        let hir = HIR {
            entry: self.entry,
            blocks: self.completed,
        };
        (hir, returns)
    }

    /// Get current block's id.
    pub fn current_block_id(&self) -> BlockId {
        self.current.id
    }

    /// Instruction id for a terminal
    pub fn next_instruction_id(&mut self) -> InstructionId {
        self.env.new_instruction_id()
    }
}
