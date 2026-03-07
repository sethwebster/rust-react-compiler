#![allow(unused_imports, unused_variables, dead_code)]

use std::collections::{HashMap, HashSet};
use indexmap::IndexMap;

use crate::hir::hir::{
    ArrayElement, BasicBlock, BlockId, DeclarationId, Effect, HIR, HIRFunction, Identifier,
    IdentifierId, InstructionId, InstructionValue, LValue, MutableRange, ObjectPatternProperty,
    Param, Pattern, Phi, Place, SourceLocation, SpreadPattern, Type,
};
use crate::hir::environment::Environment;

/// Define (SSA-rename) all definition places inside a Destructure pattern.
fn define_pattern_places(pattern: &mut Pattern, builder: &mut SsaBuilder) {
    match pattern {
        Pattern::Array(ap) => {
            for item in &mut ap.items {
                match item {
                    ArrayElement::Place(p) => {
                        *p = builder.define_place(p.clone());
                    }
                    ArrayElement::Spread(s) => {
                        s.place = builder.define_place(s.place.clone());
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(op) => {
            for prop in &mut op.properties {
                match prop {
                    ObjectPatternProperty::Property(p) => {
                        p.place = builder.define_place(p.place.clone());
                    }
                    ObjectPatternProperty::Spread(s) => {
                        s.place = builder.define_place(s.place.clone());
                    }
                }
            }
        }
    }
}

/// Visit all definition-place identifiers inside a Destructure pattern.
fn visit_pattern_places(pattern: &Pattern, visit: &mut impl FnMut(IdentifierId)) {
    match pattern {
        Pattern::Array(ap) => {
            for item in &ap.items {
                match item {
                    ArrayElement::Place(p) => visit(p.identifier),
                    ArrayElement::Spread(s) => visit(s.place.identifier),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(op) => {
            for prop in &op.properties {
                match prop {
                    ObjectPatternProperty::Property(p) => visit(p.place.identifier),
                    ObjectPatternProperty::Spread(s) => visit(s.place.identifier),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State per basic block: holds the current SSA renaming for old -> new ids,
// and incomplete phis that need to be filled once all preds are sealed.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct BlockState {
    /// Maps old IdentifierId -> current SSA IdentifierId for this block.
    defs: HashMap<IdentifierId, IdentifierId>,
    /// Phis inserted speculatively while predecessors were unvisited.
    incomplete_phis: Vec<IncompletePhi>,
}

impl BlockState {
    fn new() -> Self {
        BlockState {
            defs: HashMap::new(),
            incomplete_phis: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct IncompletePhi {
    /// The original (pre-SSA) place being renamed.
    old_id: IdentifierId,
    /// The new SSA place created as a placeholder.
    new_id: IdentifierId,
    /// The block this incomplete phi lives in.
    block_id: BlockId,
}

// ---------------------------------------------------------------------------
// ID allocator: seeds above the max existing IdentifierId in the function.
// ---------------------------------------------------------------------------

fn find_max_identifier_id(hir: &HIR) -> u32 {
    let mut max = 0u32;

    let check = |id: IdentifierId, max: &mut u32| {
        if id.0 > *max {
            *max = id.0;
        }
    };

    for block in hir.blocks.values() {
        for phi in &block.phis {
            check(phi.place.identifier, &mut max);
            for p in phi.operands.values() {
                check(p.identifier, &mut max);
            }
        }
        for instr in &block.instructions {
            check(instr.lvalue.identifier, &mut max);
            collect_operand_ids_from_value(&instr.value, &mut |id| {
                check(id, &mut max);
            });
        }
    }
    max
}

fn collect_operand_ids_from_value(val: &InstructionValue, visit: &mut impl FnMut(IdentifierId)) {
    use InstructionValue::*;
    match val {
        LoadLocal { place, .. } | LoadContext { place, .. } => visit(place.identifier),
        DeclareLocal { lvalue, .. } => visit(lvalue.place.identifier),
        DeclareContext { lvalue, .. } => visit(lvalue.place.identifier),
        StoreLocal { lvalue, value, .. } => {
            visit(lvalue.place.identifier);
            visit(value.identifier);
        }
        StoreContext { lvalue, value, .. } => {
            visit(lvalue.place.identifier);
            visit(value.identifier);
        }
        StoreGlobal { value, .. } => visit(value.identifier),
        Destructure { lvalue, value, .. } => {
            visit(value.identifier);
            // Also visit pattern definition places.
            visit_pattern_places(&lvalue.pattern, &mut |id| visit(id));
        }
        BinaryExpression { left, right, .. } => {
            visit(left.identifier);
            visit(right.identifier);
        }
        TernaryExpression { test, consequent, alternate, .. } => {
            visit(test.identifier);
            visit(consequent.identifier);
            visit(alternate.identifier);
        }
        UnaryExpression { value, .. } => visit(value.identifier),
        TypeCastExpression { value, .. } => visit(value.identifier),
        CallExpression { callee, args, .. } => {
            visit(callee.identifier);
            for arg in args {
                match arg {
                    crate::hir::hir::CallArg::Place(p) => visit(p.identifier),
                    crate::hir::hir::CallArg::Spread(s) => visit(s.place.identifier),
                }
            }
        }
        MethodCall { receiver, property, args, .. } => {
            visit(receiver.identifier);
            visit(property.identifier);
            for arg in args {
                match arg {
                    crate::hir::hir::CallArg::Place(p) => visit(p.identifier),
                    crate::hir::hir::CallArg::Spread(s) => visit(s.place.identifier),
                }
            }
        }
        NewExpression { callee, args, .. } => {
            visit(callee.identifier);
            for arg in args {
                match arg {
                    crate::hir::hir::CallArg::Place(p) => visit(p.identifier),
                    crate::hir::hir::CallArg::Spread(s) => visit(s.place.identifier),
                }
            }
        }
        ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    crate::hir::hir::ObjectExpressionProperty::Property(p) => {
                        visit(p.place.identifier);
                        if let crate::hir::hir::ObjectPropertyKey::Computed(k) = &p.key {
                            visit(k.identifier);
                        }
                    }
                    crate::hir::hir::ObjectExpressionProperty::Spread(s) => {
                        visit(s.place.identifier)
                    }
                }
            }
        }
        ArrayExpression { elements, .. } => {
            for el in elements {
                match el {
                    crate::hir::hir::ArrayElement::Place(p) => visit(p.identifier),
                    crate::hir::hir::ArrayElement::Spread(s) => visit(s.place.identifier),
                    crate::hir::hir::ArrayElement::Hole => {}
                }
            }
        }
        PropertyLoad { object, .. } => visit(object.identifier),
        PropertyStore { object, value, .. } => {
            visit(object.identifier);
            visit(value.identifier);
        }
        PropertyDelete { object, .. } => visit(object.identifier),
        ComputedLoad { object, property, .. } => {
            visit(object.identifier);
            visit(property.identifier);
        }
        ComputedStore { object, property, value, .. } => {
            visit(object.identifier);
            visit(property.identifier);
            visit(value.identifier);
        }
        ComputedDelete { object, property, .. } => {
            visit(object.identifier);
            visit(property.identifier);
        }
        JsxExpression { tag, props, children, .. } => {
            if let crate::hir::hir::JsxTag::Place(p) = tag {
                visit(p.identifier);
            }
            for attr in props {
                match attr {
                    crate::hir::hir::JsxAttribute::Attribute { place, .. } => {
                        visit(place.identifier)
                    }
                    crate::hir::hir::JsxAttribute::Spread { argument } => {
                        visit(argument.identifier)
                    }
                }
            }
            if let Some(ch) = children {
                for c in ch {
                    visit(c.identifier);
                }
            }
        }
        JsxFragment { children, .. } => {
            for c in children {
                visit(c.identifier);
            }
        }
        TemplateLiteral { subexprs, .. } => {
            for e in subexprs {
                visit(e.identifier);
            }
        }
        TaggedTemplateExpression { tag, .. } => visit(tag.identifier),
        Await { value, .. } => visit(value.identifier),
        GetIterator { collection, .. } => visit(collection.identifier),
        IteratorNext { iterator, collection, .. } => {
            visit(iterator.identifier);
            visit(collection.identifier);
        }
        NextPropertyOf { value, .. } => visit(value.identifier),
        PrefixUpdate { lvalue, value, .. } => {
            visit(lvalue.identifier);
            visit(value.identifier);
        }
        PostfixUpdate { lvalue, value, .. } => {
            visit(lvalue.identifier);
            visit(value.identifier);
        }
        FinishMemoize { decl, .. } => visit(decl.identifier),
        // No operand places:
        LoadGlobal { .. }
        | Primitive { .. }
        | JsxText { .. }
        | RegExpLiteral { .. }
        | MetaProperty { .. }
        | Debugger { .. }
        | StartMemoize { .. }
        | InlineJs { .. }
        | UnsupportedNode { .. }
        | ObjectMethod { .. }
        | FunctionExpression { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// SSA builder: mirrors SSABuilder in EnterSSA.ts
// ---------------------------------------------------------------------------

struct SsaBuilder {
    /// Per-block SSA state.
    states: HashMap<BlockId, BlockState>,
    /// Current block being processed.
    current: Option<BlockId>,
    /// How many unsealed predecessors each block still has.
    /// When count reaches 0 and block has been visited, fix incomplete phis.
    unsealed_preds: HashMap<BlockId, i32>,
    /// Identifiers seen at entry with no prior definition (globals / upvalues).
    unknown: HashSet<IdentifierId>,
    /// Context identifiers (captured variables, not renamed again).
    context: HashSet<IdentifierId>,
    /// Next fresh SSA id.
    next_id: u32,
    /// Snapshot of identifier metadata at SSA entry time (for copying to new SSA IDs).
    ident_snapshot: HashMap<IdentifierId, Identifier>,
    /// New identifiers registered during SSA to be written back to env.
    new_identifiers: Vec<Identifier>,
}

impl SsaBuilder {
    fn new(max_existing_id: u32, ident_snapshot: HashMap<IdentifierId, Identifier>) -> Self {
        SsaBuilder {
            states: HashMap::new(),
            current: None,
            unsealed_preds: HashMap::new(),
            unknown: HashSet::new(),
            context: HashSet::new(),
            next_id: max_existing_id + 1,
            ident_snapshot,
            new_identifiers: Vec::new(),
        }
    }

    fn alloc_id(&mut self) -> IdentifierId {
        let id = IdentifierId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Allocate a new SSA id for a phi node, copying name metadata from old_id.
    /// Unlike `define_place`, this is used for phi-result IDs which need to be
    /// registered in new_identifiers so codegen can find their names.
    fn alloc_phi_id(&mut self, old_id: IdentifierId) -> IdentifierId {
        let new_id = self.alloc_id();
        let new_ident = if let Some(orig) = self.ident_snapshot.get(&old_id) {
            let mut copy = orig.clone();
            copy.id = new_id;
            copy.declaration_id = orig.declaration_id;
            copy
        } else {
            Identifier {
                id: new_id,
                declaration_id: DeclarationId(new_id.0),
                name: None,
                mutable_range: MutableRange::zero(),
                scope: None,
                type_: Type::default(),
                loc: SourceLocation::Generated,
            }
        };
        self.new_identifiers.push(new_ident);
        new_id
    }

    fn state_mut(&mut self) -> &mut BlockState {
        let block_id = self.current.expect("must be inside a block");
        self.states.get_mut(&block_id).expect("state must exist for current block")
    }

    fn start_block(&mut self, block_id: BlockId) {
        self.current = Some(block_id);
        self.states.entry(block_id).or_insert_with(BlockState::new);
    }

    /// Returns the current SSA id for `old_id` in the current block,
    /// walking predecessor chains if needed (may insert phi nodes).
    fn get_id_at(
        &mut self,
        old_id: IdentifierId,
        block_id: BlockId,
        blocks: &mut IndexMap<BlockId, BasicBlock>,
    ) -> IdentifierId {
        // Check if defined in this block's state.
        if let Some(&new_id) = self.states.get(&block_id).and_then(|s| s.defs.get(&old_id)) {
            return new_id;
        }

        let pred_count = blocks[&block_id].preds.len();

        if pred_count == 0 {
            // Entry block: variable is undefined (global/upvalue).
            self.unknown.insert(old_id);
            return old_id;
        }

        let unsealed = self.unsealed_preds.get(&block_id).copied().unwrap_or(0);
        if unsealed > 0 {
            // Block still has unvisited predecessors; insert incomplete phi.
            let new_id = self.alloc_phi_id(old_id);
            let state = self.states.get_mut(&block_id).expect("state must exist");
            state.incomplete_phis.push(IncompletePhi {
                old_id,
                new_id,
                block_id,
            });
            state.defs.insert(old_id, new_id);
            return new_id;
        }

        if pred_count == 1 {
            // Single predecessor: forward the lookup.
            let pred = *blocks[&block_id].preds.iter().next().unwrap();
            let new_id = self.get_id_at(old_id, pred, blocks);
            self.states.get_mut(&block_id).expect("state must exist").defs.insert(old_id, new_id);
            return new_id;
        }

        // Multiple predecessors: may need a phi.
        let new_id = self.alloc_phi_id(old_id);
        // Register before recursing to break cycles from loops.
        self.states.get_mut(&block_id).expect("state must exist").defs.insert(old_id, new_id);
        self.add_phi(block_id, old_id, new_id, blocks);
        new_id
    }

    /// Collect the per-predecessor operands and push a Phi node into `block_id`.
    fn add_phi(
        &mut self,
        block_id: BlockId,
        old_id: IdentifierId,
        new_id: IdentifierId,
        blocks: &mut IndexMap<BlockId, BasicBlock>,
    ) {
        let preds: Vec<BlockId> = blocks[&block_id].preds.iter().copied().collect();
        let mut operands: HashMap<BlockId, Place> = HashMap::new();
        for pred_id in preds {
            let pred_new_id = self.get_id_at(old_id, pred_id, blocks);
            // Borrow a representative Place from the predecessor block to clone
            // the effect/reactive/loc metadata. Fall back to Generated.
            let template_place = Place {
                identifier: pred_new_id,
                effect: Effect::Unknown,
                reactive: false,
                loc: SourceLocation::Generated,
            };
            operands.insert(pred_id, template_place);
        }
        let phi = Phi {
            place: Place {
                identifier: new_id,
                effect: Effect::Unknown,
                reactive: false,
                loc: SourceLocation::Generated,
            },
            operands,
        };
        // Only add if not already present (idempotent).
        let block = blocks.get_mut(&block_id).expect("block must exist");
        if !block.phis.iter().any(|p| p.place.identifier == new_id) {
            block.phis.push(phi);
        }
    }

    /// After all predecessors have been seen, fill in the incomplete phis.
    fn fix_incomplete_phis(
        &mut self,
        block_id: BlockId,
        blocks: &mut IndexMap<BlockId, BasicBlock>,
    ) {
        // Drain incomplete_phis for this block so we can iterate without
        // holding a mutable reference to self.states simultaneously.
        let incomplete = match self.states.get_mut(&block_id) {
            Some(s) => std::mem::take(&mut s.incomplete_phis),
            None => return,
        };
        for ip in incomplete {
            self.add_phi(block_id, ip.old_id, ip.new_id, blocks);
        }
    }

    /// Rename a use-site Place: looks up the current SSA id.
    fn get_place(
        &mut self,
        place: Place,
        blocks: &mut IndexMap<BlockId, BasicBlock>,
    ) -> Place {
        let current = self.current.expect("must be in a block");
        let new_id = self.get_id_at(place.identifier, current, blocks);
        Place { identifier: new_id, ..place }
    }

    /// Rename a definition-site Place: allocates a fresh SSA id.
    /// Context places (captured vars) are not redefined.
    fn define_place(&mut self, place: Place) -> Place {
        if self.context.contains(&place.identifier) {
            let state = self.state_mut();
            let existing = state.defs.get(&place.identifier).copied().unwrap_or(place.identifier);
            return Place { identifier: existing, ..place };
        }
        let new_id = self.alloc_id();
        let old_id = place.identifier;
        self.state_mut().defs.insert(old_id, new_id);
        // Register new identifier copying metadata from original (if available).
        let new_ident = if let Some(orig) = self.ident_snapshot.get(&old_id) {
            let mut copy = orig.clone();
            copy.id = new_id;
            copy.declaration_id = orig.declaration_id; // keep same decl group
            copy
        } else {
            Identifier {
                id: new_id,
                declaration_id: DeclarationId(new_id.0),
                name: None,
                mutable_range: MutableRange::zero(),
                scope: None,
                type_: Type::default(),
                loc: place.loc.clone(),
            }
        };
        self.new_identifiers.push(new_ident);
        Place { identifier: new_id, ..place }
    }

    /// Mark an identifier as context (captured from outer scope).
    fn define_context_place(&mut self, place: Place) -> Place {
        let old_id = place.identifier;
        let new_place = self.define_place(place);
        self.context.insert(old_id);
        new_place
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn enter_ssa(hir: &mut HIRFunction) {
    enter_ssa_with_env(hir, None);
}

pub fn enter_ssa_with_env(hir: &mut HIRFunction, env: Option<&mut Environment>) {
    fix_predecessors(&mut hir.body);

    let max_id = find_max_identifier_id(&hir.body);
    let snapshot: HashMap<IdentifierId, Identifier> = env
        .as_ref()
        .map(|e| e.identifiers.clone())
        .unwrap_or_default();
    let mut builder = SsaBuilder::new(max_id, snapshot);

    let entry = hir.body.entry;

    // Rename parameters at the entry block.
    builder.start_block(entry);

    // Context places (outer captures) are defined first as context.
    // For root function, context should be empty.
    for ctx_place in hir.context.iter_mut() {
        *ctx_place = builder.define_context_place(ctx_place.clone());
    }

    // Rename params.
    for param in hir.params.iter_mut() {
        match param {
            Param::Place(p) => {
                *p = builder.define_place(p.clone());
            }
            Param::Spread(SpreadPattern { place }) => {
                *place = builder.define_place(place.clone());
            }
        }
    }

    // Collect block ids in reverse-postorder (already stored that way in IndexMap).
    let block_ids: Vec<BlockId> = hir.body.blocks.keys().copied().collect();

    // Initialize unsealed counts: for each block count how many predecessors
    // have not yet been processed. We do this lazily during the main loop.
    // The TS code tracks `unsealedPreds` as predecessor-count - already-seen-preds.

    let mut visited: HashSet<BlockId> = HashSet::new();

    for block_id in block_ids {
        if block_id != entry {
            builder.start_block(block_id);
        }

        visited.insert(block_id);

        // --- Rename phi result places (define new SSA ids for phi outputs) ---
        // The TS doesn't rename phi places here in the loop; phi results are
        // added on-demand via addPhi. We skip phi renaming here since phis are
        // added lazily by get_id_at / add_phi.

        // --- Rename instruction operands then lvalues ---
        // We need to take instructions out to avoid borrow conflicts.
        let mut instrs = std::mem::take(&mut hir.body.blocks.get_mut(&block_id).unwrap().instructions);
        for instr in &mut instrs {
            // Rename operands (use sites) first.
            rename_operands_in_value(&mut instr.value, &mut builder, &mut hir.body.blocks);

            // Rename lvalue (definition site).
            instr.lvalue = builder.define_place(instr.lvalue.clone());

            // For Destructure instructions, also define the pattern places
            // (these are additional definition sites inside the pattern).
            if let InstructionValue::Destructure { lvalue, .. } = &mut instr.value {
                define_pattern_places(&mut lvalue.pattern, &mut builder);
            }
        }
        hir.body.blocks.get_mut(&block_id).unwrap().instructions = instrs;

        // --- Rename terminal operands ---
        {
            let mut terminal = std::mem::replace(
                &mut hir.body.blocks.get_mut(&block_id).unwrap().terminal,
                crate::hir::hir::Terminal::Unreachable {
                    id: crate::hir::hir::make_instruction_id(0),
                    loc: SourceLocation::Generated,
                },
            );
            rename_terminal_operands(&mut terminal, &mut builder, &mut hir.body.blocks);
            hir.body.blocks.get_mut(&block_id).unwrap().terminal = terminal;
        }

        // --- Update unsealed counts for successors ---
        let succs: Vec<BlockId> = hir.body.blocks[&block_id].terminal.successors();
        for succ in succs {
            let count = if let Some(c) = builder.unsealed_preds.get(&succ) {
                *c - 1
            } else {
                // Initial count = number of predecessors - 1 (this is the first pred we see)
                let pred_count = hir.body.blocks.get(&succ).map(|b| b.preds.len()).unwrap_or(1);
                (pred_count as i32) - 1
            };
            builder.unsealed_preds.insert(succ, count);

            // If count is 0 and the successor has already been visited, fix its incomplete phis.
            if count == 0 && visited.contains(&succ) {
                builder.fix_incomplete_phis(succ, &mut hir.body.blocks);
            }
        }
    }

    // Fix up pre-existing phi operands that were created before SSA (e.g. by
    // lower_logical for &&/||/?? expressions).  Their operand identifiers still
    // reference pre-SSA ids; we resolve each one via get_id_at so they match the
    // SSA-renamed instruction lvalues.
    let final_block_ids: Vec<BlockId> = hir.body.blocks.keys().copied().collect();
    for block_id in final_block_ids {
        let phi_count = hir.body.blocks[&block_id].phis.len();
        // For each pre-existing phi, collect (pred_id, old_id) pairs then resolve.
        for phi_idx in 0..phi_count {
            // Collect without holding a borrow into blocks.
            let updates: Vec<(BlockId, IdentifierId)> = hir.body.blocks[&block_id].phis[phi_idx]
                .operands
                .iter()
                .map(|(&pred, op)| (pred, op.identifier))
                .collect();

            // Resolve each operand to its SSA id.
            let resolved: Vec<(BlockId, IdentifierId)> = updates
                .into_iter()
                .map(|(pred_id, old_id)| {
                    let new_id = builder.get_id_at(old_id, pred_id, &mut hir.body.blocks);
                    (pred_id, new_id)
                })
                .collect();

            // Apply resolved ids — re-index by phi_idx (phi_count may grow if
            // get_id_at triggered add_phi, but those are appended after phi_idx).
            if let Some(phi) = hir.body.blocks.get_mut(&block_id).and_then(|b| b.phis.get_mut(phi_idx)) {
                for (pred_id, new_id) in resolved {
                    if let Some(op) = phi.operands.get_mut(&pred_id) {
                        op.identifier = new_id;
                    }
                }
            }
        }
    }

    // Write back newly created identifiers to env.
    if let Some(e) = env {
        for new_ident in builder.new_identifiers {
            e.identifiers.insert(new_ident.id, new_ident);
        }
    }
}

// ---------------------------------------------------------------------------
// Predecessor fixup
// ---------------------------------------------------------------------------

fn fix_predecessors(hir: &mut HIR) {
    let mut preds: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();

    for (&id, block) in &hir.blocks {
        for succ in block.terminal.successors() {
            preds.entry(succ).or_default().insert(id);
        }
    }

    for (&id, block) in &mut hir.blocks {
        if let Some(p) = preds.get(&id) {
            block.preds = p.clone();
        } else {
            block.preds.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// Rename operands inside an InstructionValue (use-sites only)
// ---------------------------------------------------------------------------

fn rename_operands_in_value(
    val: &mut InstructionValue,
    builder: &mut SsaBuilder,
    blocks: &mut IndexMap<BlockId, BasicBlock>,
) {
    use InstructionValue::*;

    // Helper closure: rename a single Place in-place.
    macro_rules! rp {
        ($place:expr) => {
            *$place = builder.get_place((*$place).clone(), blocks)
        };
    }

    match val {
        LoadLocal { place, .. } | LoadContext { place, .. } => rp!(place),

        StoreLocal { value, .. } => rp!(value),
        StoreContext { value, .. } => rp!(value),
        StoreGlobal { value, .. } => rp!(value),
        Destructure { value, .. } => rp!(value),

        BinaryExpression { left, right, .. } => {
            rp!(left);
            rp!(right);
        }
        TernaryExpression { test, consequent, alternate, .. } => {
            rp!(test);
            rp!(consequent);
            rp!(alternate);
        }
        UnaryExpression { value, .. } => rp!(value),
        TypeCastExpression { value, .. } => rp!(value),

        CallExpression { callee, args, .. } => {
            rp!(callee);
            for arg in args.iter_mut() {
                match arg {
                    crate::hir::hir::CallArg::Place(p) => rp!(p),
                    crate::hir::hir::CallArg::Spread(s) => rp!(&mut s.place),
                }
            }
        }
        MethodCall { receiver, property, args, .. } => {
            rp!(receiver);
            rp!(property);
            for arg in args.iter_mut() {
                match arg {
                    crate::hir::hir::CallArg::Place(p) => rp!(p),
                    crate::hir::hir::CallArg::Spread(s) => rp!(&mut s.place),
                }
            }
        }
        NewExpression { callee, args, .. } => {
            rp!(callee);
            for arg in args.iter_mut() {
                match arg {
                    crate::hir::hir::CallArg::Place(p) => rp!(p),
                    crate::hir::hir::CallArg::Spread(s) => rp!(&mut s.place),
                }
            }
        }

        ObjectExpression { properties, .. } => {
            for prop in properties.iter_mut() {
                match prop {
                    crate::hir::hir::ObjectExpressionProperty::Property(p) => {
                        rp!(&mut p.place);
                        if let crate::hir::hir::ObjectPropertyKey::Computed(k) = &mut p.key {
                            rp!(k);
                        }
                    }
                    crate::hir::hir::ObjectExpressionProperty::Spread(s) => rp!(&mut s.place),
                }
            }
        }
        ArrayExpression { elements, .. } => {
            for el in elements.iter_mut() {
                match el {
                    crate::hir::hir::ArrayElement::Place(p) => rp!(p),
                    crate::hir::hir::ArrayElement::Spread(s) => rp!(&mut s.place),
                    crate::hir::hir::ArrayElement::Hole => {}
                }
            }
        }

        PropertyLoad { object, .. } => rp!(object),
        PropertyStore { object, value, .. } => {
            rp!(object);
            rp!(value);
        }
        PropertyDelete { object, .. } => rp!(object),
        ComputedLoad { object, property, .. } => {
            rp!(object);
            rp!(property);
        }
        ComputedStore { object, property, value, .. } => {
            rp!(object);
            rp!(property);
            rp!(value);
        }
        ComputedDelete { object, property, .. } => {
            rp!(object);
            rp!(property);
        }

        JsxExpression { tag, props, children, .. } => {
            if let crate::hir::hir::JsxTag::Place(p) = tag {
                rp!(p);
            }
            for attr in props.iter_mut() {
                match attr {
                    crate::hir::hir::JsxAttribute::Attribute { place, .. } => rp!(place),
                    crate::hir::hir::JsxAttribute::Spread { argument } => rp!(argument),
                }
            }
            if let Some(ch) = children.as_mut() {
                for c in ch.iter_mut() {
                    rp!(c);
                }
            }
        }
        JsxFragment { children, .. } => {
            for c in children.iter_mut() {
                rp!(c);
            }
        }

        TemplateLiteral { subexprs, .. } => {
            for e in subexprs.iter_mut() {
                rp!(e);
            }
        }
        TaggedTemplateExpression { tag, .. } => rp!(tag),
        Await { value, .. } => rp!(value),
        GetIterator { collection, .. } => rp!(collection),
        IteratorNext { iterator, collection, .. } => {
            rp!(iterator);
            rp!(collection);
        }
        NextPropertyOf { value, .. } => rp!(value),
        PrefixUpdate { lvalue, value, .. } => {
            rp!(lvalue);
            rp!(value);
        }
        PostfixUpdate { lvalue, value, .. } => {
            rp!(lvalue);
            rp!(value);
        }
        FinishMemoize { decl, .. } => rp!(decl),

        // Rename context places captured by function expressions.
        FunctionExpression { lowered_func, .. } => {
            for ctx_place in lowered_func.func.context.iter_mut() {
                rp!(ctx_place);
            }
        }

        // No use-site places to rename:
        DeclareLocal { .. }
        | DeclareContext { .. }
        | LoadGlobal { .. }
        | Primitive { .. }
        | JsxText { .. }
        | RegExpLiteral { .. }
        | MetaProperty { .. }
        | Debugger { .. }
        | StartMemoize { .. }
        | InlineJs { .. }
        | UnsupportedNode { .. }
        | ObjectMethod { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Rename terminal operands (use-sites in terminal nodes)
// ---------------------------------------------------------------------------

fn rename_terminal_operands(
    terminal: &mut crate::hir::hir::Terminal,
    builder: &mut SsaBuilder,
    blocks: &mut IndexMap<BlockId, BasicBlock>,
) {
    use crate::hir::hir::Terminal::*;

    macro_rules! rp {
        ($place:expr) => {
            *$place = builder.get_place((*$place).clone(), blocks)
        };
    }

    match terminal {
        Return { value, .. } => rp!(value),
        Throw { value, .. } => rp!(value),
        If { test, .. } | Branch { test, .. } => rp!(test),
        Switch { test, cases, .. } => {
            rp!(test);
            for case in cases.iter_mut() {
                if let Some(t) = case.test.as_mut() {
                    rp!(t);
                }
            }
        }
        Try { handler_binding, .. } => {
            if let Some(p) = handler_binding.as_mut() {
                rp!(p);
            }
        }
        // Terminals with no operand Places:
        Unsupported { .. }
        | Unreachable { .. }
        | Goto { .. }
        | DoWhile { .. }
        | While { .. }
        | For { .. }
        | ForOf { .. }
        | ForIn { .. }
        | Logical { .. }
        | Ternary { .. }
        | Optional { .. }
        | Label { .. }
        | Sequence { .. }
        | MaybeThrow { .. }
        | ReactiveScope { .. }
        | PrunedScope { .. } => {}
    }
}
