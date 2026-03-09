use std::collections::{HashMap, HashSet};
use indexmap::IndexMap;
use crate::inference::aliasing_effects::AliasingEffect;

// ---------------------------------------------------------------------------
// Opaque ID types
// ---------------------------------------------------------------------------

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        pub struct $name(pub u32);

        impl $name {
            pub fn as_u32(self) -> u32 { self.0 }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

opaque_id!(IdentifierId);
opaque_id!(DeclarationId);
opaque_id!(BlockId);
opaque_id!(ScopeId);
opaque_id!(InstructionId);

pub fn make_instruction_id(n: u32) -> InstructionId { InstructionId(n) }
pub fn make_declaration_id(n: u32) -> DeclarationId { DeclarationId(n) }

// ---------------------------------------------------------------------------
// Source location
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SourceLocation {
    Generated,
    Source(Span),
}

impl SourceLocation {
    pub fn source(start: u32, end: u32) -> Self {
        SourceLocation::Source(Span { start, end })
    }
}

impl Default for SourceLocation {
    fn default() -> Self { SourceLocation::Generated }
}

// ---------------------------------------------------------------------------
// Effects and value kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Effect {
    Unknown,
    Freeze,
    Read,
    Capture,
    ConditionallyMutateIterator,
    ConditionallyMutate,
    Mutate,
    Store,
}

impl Default for Effect {
    fn default() -> Self { Effect::Unknown }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ValueKind {
    MaybeFrozen,
    Frozen,
    Primitive,
    Global,
    Mutable,
    Context,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ValueReason {
    Global,
    JsxCaptured,
    HookCaptured,
    HookReturn,
    Effect,
    KnownReturnSignature,
    Context,
    State,
    ReducerState,
    ReactiveFunctionArgument,
    Other,
}

// ---------------------------------------------------------------------------
// Mutable range
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MutableRange {
    pub start: InstructionId,
    pub end: InstructionId,
}

impl MutableRange {
    pub fn zero() -> Self {
        MutableRange {
            start: InstructionId(0),
            end: InstructionId(0),
        }
    }
}

// ---------------------------------------------------------------------------
// Identifier
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum IdentifierName {
    Named(String),
    Promoted(String),
}

impl IdentifierName {
    pub fn value(&self) -> &str {
        match self {
            IdentifierName::Named(s) | IdentifierName::Promoted(s) => s,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Identifier {
    pub id: IdentifierId,
    pub declaration_id: DeclarationId,
    pub name: Option<IdentifierName>,
    pub mutable_range: MutableRange,
    pub scope: Option<ScopeId>,
    pub type_: Type,
    pub loc: SourceLocation,
}

impl Identifier {
    pub fn new_temporary(id: IdentifierId, declaration_id: DeclarationId, loc: SourceLocation) -> Self {
        Identifier {
            id,
            declaration_id,
            name: None,
            mutable_range: MutableRange::zero(),
            scope: None,
            type_: Type::default(),
            loc,
        }
    }
}

// ---------------------------------------------------------------------------
// Place
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Place {
    pub identifier: IdentifierId,
    pub effect: Effect,
    pub reactive: bool,
    pub loc: SourceLocation,
}

impl Place {
    pub fn new(identifier: IdentifierId, loc: SourceLocation) -> Self {
        Place {
            identifier,
            effect: Effect::Unknown,
            reactive: false,
            loc,
        }
    }
}

// ---------------------------------------------------------------------------
// Type (forward-declare; full definition in types.rs)
// ---------------------------------------------------------------------------

pub use crate::hir::types::Type;

// ---------------------------------------------------------------------------
// Reactive scope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveScopeDependency {
    pub place: Place,
    pub path: Vec<DependencyPathEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DependencyPathEntry {
    pub property: String,
    pub optional: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveScopeDeclaration {
    pub identifier: IdentifierId,
    pub scope: ScopeId,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveScope {
    pub id: ScopeId,
    pub range: MutableRange,
    pub dependencies: Vec<ReactiveScopeDependency>,
    pub declarations: HashMap<IdentifierId, ReactiveScopeDeclaration>,
    pub reassignments: Vec<IdentifierId>,
    pub merged_ranges: Vec<MutableRange>,
    pub early_returns: Vec<EarlyReturn>,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EarlyReturn {
    pub loc: SourceLocation,
    pub label_id: BlockId,
}

// ---------------------------------------------------------------------------
// Phi node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Phi {
    pub place: Place,
    pub operands: HashMap<BlockId, Place>,
}

// ---------------------------------------------------------------------------
// Instruction kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum InstructionKind {
    Const,
    Let,
    Reassign,
    Catch,
    HoistedConst,
    HoistedLet,
    HoistedFunction,
    Function,
}

// ---------------------------------------------------------------------------
// Goto variant
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum GotoVariant {
    Break,
    Continue,
    Try,
}

// ---------------------------------------------------------------------------
// Block kind
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BlockKind {
    Block,
    Value,
    Loop,
    Sequence,
    Catch,
}

// ---------------------------------------------------------------------------
// Patterns & destructuring
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpreadPattern {
    pub place: Place,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hole;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ArrayElement {
    Place(Place),
    Spread(SpreadPattern),
    Hole,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArrayPattern {
    pub items: Vec<ArrayElement>,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ObjectPropertyKey {
    String(String),
    Identifier(String),
    Computed(Place),
    Number(f64),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObjectProperty {
    pub key: ObjectPropertyKey,
    pub type_: ObjectPropertyType,
    pub place: Place,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ObjectPropertyType {
    Property,
    Method,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ObjectPatternProperty {
    Property(ObjectProperty),
    Spread(SpreadPattern),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObjectPattern {
    pub properties: Vec<ObjectPatternProperty>,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Pattern {
    Array(ArrayPattern),
    Object(ObjectPattern),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LValue {
    pub place: Place,
    pub kind: InstructionKind,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LValuePattern {
    pub pattern: Pattern,
    pub kind: InstructionKind,
}

// ---------------------------------------------------------------------------
// Non-local bindings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum NonLocalBinding {
    ImportDefault { name: String, module: String },
    ImportNamespace { name: String, module: String },
    ImportSpecifier { name: String, module: String, imported: String },
    ModuleLocal { name: String },
    Global { name: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum VariableBinding {
    Identifier { identifier: IdentifierId, binding_kind: BindingKind },
    NonLocal(NonLocalBinding),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BindingKind {
    Const,
    Let,
    Var,
    Param,
    CatchClause,
    Module,
    Unknown,
}

// ---------------------------------------------------------------------------
// Property literal
// ---------------------------------------------------------------------------

pub type PropertyLiteral = String;

// ---------------------------------------------------------------------------
// JSX types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BuiltinTag {
    pub name: String,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum JsxTag {
    Builtin(BuiltinTag),
    Place(Place),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum JsxAttribute {
    Spread { argument: Place },
    Attribute { name: String, place: Place },
}

// ---------------------------------------------------------------------------
// Manual memo markers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ManualMemoDependency {
    pub root: ManualMemoRoot,
    pub path: Vec<DependencyPathEntry>,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ManualMemoRoot {
    NamedLocal { place: Place, constant: bool },
    Global { identifier_name: String },
}

// ---------------------------------------------------------------------------
// Template literal quasi
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TemplateQuasi {
    pub raw: String,
    pub cooked: Option<String>,
}

// ---------------------------------------------------------------------------
// Lowered function (HIR nested functions)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LoweredFunction {
    pub func: Box<HIRFunction>,
}

// ---------------------------------------------------------------------------
// Return variant
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReturnVariant {
    Void,
    Implicit,
    Explicit,
}

// ---------------------------------------------------------------------------
// Case (for switch statements)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Case {
    pub test: Option<Place>,
    pub block: BlockId,
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Terminal {
    Unsupported {
        id: InstructionId,
        loc: SourceLocation,
    },
    Unreachable {
        id: InstructionId,
        loc: SourceLocation,
    },
    Return {
        value: Place,
        return_variant: ReturnVariant,
        id: InstructionId,
        loc: SourceLocation,
        effects: Option<Vec<AliasingEffect>>,
    },
    Throw {
        value: Place,
        id: InstructionId,
        loc: SourceLocation,
    },
    Goto {
        block: BlockId,
        variant: GotoVariant,
        id: InstructionId,
        loc: SourceLocation,
    },
    If {
        test: Place,
        consequent: BlockId,
        alternate: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Branch {
        test: Place,
        consequent: BlockId,
        alternate: BlockId,
        fallthrough: BlockId,
        /// Set when this Branch encodes a logical expression (&&, ||, ??).
        /// None when used as a loop condition.
        logical_op: Option<LogicalOperator>,
        id: InstructionId,
        loc: SourceLocation,
    },
    Switch {
        test: Place,
        cases: Vec<Case>,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    DoWhile {
        loop_: BlockId,
        test: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    While {
        test: BlockId,
        loop_: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    For {
        init: BlockId,
        test: BlockId,
        update: Option<BlockId>,
        loop_: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    ForOf {
        init: BlockId,
        test: BlockId,
        loop_: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    ForIn {
        init: BlockId,
        loop_: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Logical {
        operator: LogicalOperator,
        test: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Ternary {
        test: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Optional {
        optional: bool,
        test: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Label {
        block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    Sequence {
        block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    MaybeThrow {
        continuation: BlockId,
        handler: Option<BlockId>,
        id: InstructionId,
        loc: SourceLocation,
        effects: Option<Vec<AliasingEffect>>,
    },
    Try {
        block: BlockId,
        handler_binding: Option<Place>,
        handler: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    ReactiveScope {
        scope: ReactiveScope,
        block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
    PrunedScope {
        scope: ReactiveScope,
        block: BlockId,
        fallthrough: BlockId,
        id: InstructionId,
        loc: SourceLocation,
    },
}

impl Terminal {
    pub fn id(&self) -> InstructionId {
        match self {
            Terminal::Unsupported { id, .. }
            | Terminal::Unreachable { id, .. }
            | Terminal::Return { id, .. }
            | Terminal::Throw { id, .. }
            | Terminal::Goto { id, .. }
            | Terminal::If { id, .. }
            | Terminal::Branch { id, .. }
            | Terminal::Switch { id, .. }
            | Terminal::DoWhile { id, .. }
            | Terminal::While { id, .. }
            | Terminal::For { id, .. }
            | Terminal::ForOf { id, .. }
            | Terminal::ForIn { id, .. }
            | Terminal::Logical { id, .. }
            | Terminal::Ternary { id, .. }
            | Terminal::Optional { id, .. }
            | Terminal::Label { id, .. }
            | Terminal::Sequence { id, .. }
            | Terminal::MaybeThrow { id, .. }
            | Terminal::Try { id, .. }
            | Terminal::ReactiveScope { id, .. }
            | Terminal::PrunedScope { id, .. } => *id,
        }
    }

    pub fn loc(&self) -> &SourceLocation {
        match self {
            Terminal::Unsupported { loc, .. }
            | Terminal::Unreachable { loc, .. }
            | Terminal::Return { loc, .. }
            | Terminal::Throw { loc, .. }
            | Terminal::Goto { loc, .. }
            | Terminal::If { loc, .. }
            | Terminal::Branch { loc, .. }
            | Terminal::Switch { loc, .. }
            | Terminal::DoWhile { loc, .. }
            | Terminal::While { loc, .. }
            | Terminal::For { loc, .. }
            | Terminal::ForOf { loc, .. }
            | Terminal::ForIn { loc, .. }
            | Terminal::Logical { loc, .. }
            | Terminal::Ternary { loc, .. }
            | Terminal::Optional { loc, .. }
            | Terminal::Label { loc, .. }
            | Terminal::Sequence { loc, .. }
            | Terminal::MaybeThrow { loc, .. }
            | Terminal::Try { loc, .. }
            | Terminal::ReactiveScope { loc, .. }
            | Terminal::PrunedScope { loc, .. } => loc,
        }
    }

    pub fn fallthrough(&self) -> Option<BlockId> {
        match self {
            Terminal::Unsupported { .. }
            | Terminal::Unreachable { .. }
            | Terminal::Return { .. }
            | Terminal::Throw { .. }
            | Terminal::Goto { .. }
            | Terminal::MaybeThrow { .. } => None,
            Terminal::If { fallthrough, .. }
            | Terminal::Branch { fallthrough, .. }
            | Terminal::Switch { fallthrough, .. }
            | Terminal::DoWhile { fallthrough, .. }
            | Terminal::While { fallthrough, .. }
            | Terminal::For { fallthrough, .. }
            | Terminal::ForOf { fallthrough, .. }
            | Terminal::ForIn { fallthrough, .. }
            | Terminal::Logical { fallthrough, .. }
            | Terminal::Ternary { fallthrough, .. }
            | Terminal::Optional { fallthrough, .. }
            | Terminal::Label { fallthrough, .. }
            | Terminal::Sequence { fallthrough, .. }
            | Terminal::Try { fallthrough, .. }
            | Terminal::ReactiveScope { fallthrough, .. }
            | Terminal::PrunedScope { fallthrough, .. } => Some(*fallthrough),
        }
    }

    /// Returns all successor block IDs (not including fallthrough unless it's a successor).
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Terminal::Unsupported { .. }
            | Terminal::Unreachable { .. }
            | Terminal::Return { .. }
            | Terminal::Throw { .. } => vec![],
            Terminal::Goto { block, .. } => vec![*block],
            Terminal::If { consequent, alternate, fallthrough, .. }
            | Terminal::Branch { consequent, alternate, fallthrough, .. } => {
                vec![*consequent, *alternate, *fallthrough]
            }
            Terminal::Switch { cases, fallthrough, .. } => {
                let mut succs: Vec<BlockId> = cases.iter().map(|c| c.block).collect();
                succs.push(*fallthrough);
                succs
            }
            Terminal::DoWhile { loop_, test, fallthrough, .. } => {
                vec![*loop_, *test, *fallthrough]
            }
            Terminal::While { test, loop_, fallthrough, .. } => {
                vec![*test, *loop_, *fallthrough]
            }
            Terminal::For { test, update, loop_, fallthrough, .. } => {
                // `init` == the block containing this terminal; omit to avoid self-predecessor loop.
                let mut succs = vec![*test, *loop_, *fallthrough];
                if let Some(u) = update { succs.push(*u); }
                succs
            }
            Terminal::ForOf { test, fallthrough, .. } => {
                // `init` == the block containing this terminal; the loop body (loop_)
                // is only reachable via the test block's Branch, NOT directly from init.
                // Including loop_ here would give loop_ a spurious predecessor (init),
                // causing SSA to insert phi nodes for loop-body bindings unnecessarily.
                vec![*test, *fallthrough]
            }
            Terminal::ForIn { loop_, fallthrough, .. } => {
                // ForIn: loop_ IS a direct successor of init (no separate test block).
                vec![*loop_, *fallthrough]
            }
            Terminal::Logical { test, fallthrough, .. } => vec![*test, *fallthrough],
            Terminal::Ternary { test, fallthrough, .. } => vec![*test, *fallthrough],
            Terminal::Optional { test, fallthrough, .. } => vec![*test, *fallthrough],
            Terminal::Label { block, fallthrough, .. } => vec![*block, *fallthrough],
            Terminal::Sequence { block, fallthrough, .. } => vec![*block, *fallthrough],
            Terminal::MaybeThrow { continuation, handler, .. } => {
                let mut succs = vec![*continuation];
                if let Some(h) = handler { succs.push(*h); }
                succs
            }
            Terminal::Try { block, handler, fallthrough, .. } => {
                vec![*block, *handler, *fallthrough]
            }
            Terminal::ReactiveScope { block, fallthrough, .. }
            | Terminal::PrunedScope { block, fallthrough, .. } => {
                vec![*block, *fallthrough]
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Logical operator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum LogicalOperator {
    And,
    Or,
    NullishCoalescing,
}

// ---------------------------------------------------------------------------
// Binary operator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BinaryOperator {
    Add, Sub, Mul, Div, Mod, Exp,
    BitAnd, BitOr, BitXor, Shl, Shr, UShr,
    Eq, NEq, StrictEq, StrictNEq,
    Lt, LtEq, Gt, GtEq,
    In, Instanceof,
}

// ---------------------------------------------------------------------------
// Unary operator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum UnaryOperator {
    Not,
    Minus,
    Plus,
    BitNot,
    Typeof,
    Void,
}

// ---------------------------------------------------------------------------
// Update operator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UpdateOperator {
    Increment,
    Decrement,
}

// ---------------------------------------------------------------------------
// Type annotation stub (just hold a string for now)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TypeAnnotation(pub String);

// ---------------------------------------------------------------------------
// InstructionValue
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum InstructionValue {
    // --- Loads ---
    LoadLocal {
        place: Place,
        loc: SourceLocation,
    },
    LoadContext {
        place: Place,
        loc: SourceLocation,
    },
    LoadGlobal {
        binding: NonLocalBinding,
        loc: SourceLocation,
    },

    // --- Declarations / stores ---
    DeclareLocal {
        lvalue: LValue,
        type_annotation: Option<TypeAnnotation>,
        loc: SourceLocation,
    },
    DeclareContext {
        lvalue: ContextLValue,
        loc: SourceLocation,
    },
    StoreLocal {
        lvalue: LValue,
        value: Place,
        type_annotation: Option<TypeAnnotation>,
        loc: SourceLocation,
    },
    StoreContext {
        lvalue: ContextStoreLValue,
        value: Place,
        loc: SourceLocation,
    },
    StoreGlobal {
        name: String,
        value: Place,
        loc: SourceLocation,
    },

    // --- Destructuring ---
    Destructure {
        lvalue: LValuePattern,
        value: Place,
        loc: SourceLocation,
    },

    // --- Primitives ---
    Primitive {
        value: PrimitiveValue,
        loc: SourceLocation,
    },
    JsxText {
        value: String,
        loc: SourceLocation,
    },

    // --- Expressions ---
    BinaryExpression {
        operator: BinaryOperator,
        left: Place,
        right: Place,
        loc: SourceLocation,
    },
    /// Conditional (ternary) expression: test ? consequent : alternate.
    /// Used for destructuring default lowering: `value === undefined ? default : value`.
    TernaryExpression {
        test: Place,
        consequent: Place,
        alternate: Place,
        loc: SourceLocation,
    },
    UnaryExpression {
        operator: UnaryOperator,
        value: Place,
        loc: SourceLocation,
    },
    TypeCastExpression {
        value: Place,
        type_: Type,
        source_annotation: Option<String>,
        loc: SourceLocation,
    },

    // --- Calls ---
    CallExpression {
        callee: Place,
        args: Vec<CallArg>,
        loc: SourceLocation,
    },
    MethodCall {
        receiver: Place,
        property: Place,
        args: Vec<CallArg>,
        loc: SourceLocation,
    },
    NewExpression {
        callee: Place,
        args: Vec<CallArg>,
        loc: SourceLocation,
    },

    // --- Objects / Arrays ---
    ObjectExpression {
        properties: Vec<ObjectExpressionProperty>,
        loc: SourceLocation,
    },
    ObjectMethod {
        lowered_func: LoweredFunction,
        loc: SourceLocation,
    },
    ArrayExpression {
        elements: Vec<ArrayElement>,
        loc: SourceLocation,
    },

    // --- Property access ---
    PropertyLoad {
        object: Place,
        property: PropertyLiteral,
        loc: SourceLocation,
    },
    PropertyStore {
        object: Place,
        property: PropertyLiteral,
        value: Place,
        loc: SourceLocation,
    },
    PropertyDelete {
        object: Place,
        property: PropertyLiteral,
        loc: SourceLocation,
    },
    ComputedLoad {
        object: Place,
        property: Place,
        loc: SourceLocation,
    },
    ComputedStore {
        object: Place,
        property: Place,
        value: Place,
        loc: SourceLocation,
    },
    ComputedDelete {
        object: Place,
        property: Place,
        loc: SourceLocation,
    },

    // --- JSX ---
    JsxExpression {
        tag: JsxTag,
        props: Vec<JsxAttribute>,
        children: Option<Vec<Place>>,
        loc: SourceLocation,
        opening_loc: SourceLocation,
        closing_loc: SourceLocation,
    },
    JsxFragment {
        children: Vec<Place>,
        loc: SourceLocation,
    },

    // --- Functions ---
    FunctionExpression {
        name: Option<String>,
        name_hint: Option<String>,
        lowered_func: LoweredFunction,
        fn_type: FunctionExpressionType,
        loc: SourceLocation,
    },

    // --- Templates ---
    TemplateLiteral {
        subexprs: Vec<Place>,
        quasis: Vec<TemplateQuasi>,
        loc: SourceLocation,
    },
    TaggedTemplateExpression {
        tag: Place,
        quasi: TemplateQuasi,
        loc: SourceLocation,
    },

    // --- Async ---
    Await {
        value: Place,
        loc: SourceLocation,
    },

    // --- Iterator protocol ---
    GetIterator {
        collection: Place,
        loc: SourceLocation,
    },
    IteratorNext {
        iterator: Place,
        collection: Place,
        loc: SourceLocation,
    },
    NextPropertyOf {
        value: Place,
        loc: SourceLocation,
    },

    // --- Update expressions ---
    PrefixUpdate {
        lvalue: Place,
        operation: UpdateOperator,
        value: Place,
        loc: SourceLocation,
    },
    PostfixUpdate {
        lvalue: Place,
        operation: UpdateOperator,
        value: Place,
        loc: SourceLocation,
    },

    // --- Misc ---
    RegExpLiteral {
        pattern: String,
        flags: String,
        loc: SourceLocation,
    },
    MetaProperty {
        meta: String,
        property: String,
        loc: SourceLocation,
    },
    Debugger {
        loc: SourceLocation,
    },

    // --- Manual memo markers ---
    StartMemoize {
        manual_memo_id: u32,
        deps: Option<Vec<ManualMemoDependency>>,
        deps_loc: Option<SourceLocation>,
        has_invalid_deps: bool,
        loc: SourceLocation,
    },
    FinishMemoize {
        manual_memo_id: u32,
        decl: Place,
        pruned: bool,
        loc: SourceLocation,
    },

    // --- Inline verbatim JS source (e.g. optional-chaining expressions) ---
    InlineJs {
        source: String,
        loc: SourceLocation,
    },

    // --- Fallback ---
    UnsupportedNode {
        loc: SourceLocation,
    },
}

impl InstructionValue {
    pub fn loc(&self) -> &SourceLocation {
        match self {
            InstructionValue::LoadLocal { loc, .. }
            | InstructionValue::LoadContext { loc, .. }
            | InstructionValue::LoadGlobal { loc, .. }
            | InstructionValue::DeclareLocal { loc, .. }
            | InstructionValue::DeclareContext { loc, .. }
            | InstructionValue::StoreLocal { loc, .. }
            | InstructionValue::StoreContext { loc, .. }
            | InstructionValue::StoreGlobal { loc, .. }
            | InstructionValue::Destructure { loc, .. }
            | InstructionValue::Primitive { loc, .. }
            | InstructionValue::JsxText { loc, .. }
            | InstructionValue::BinaryExpression { loc, .. }
            | InstructionValue::TernaryExpression { loc, .. }
            | InstructionValue::UnaryExpression { loc, .. }
            | InstructionValue::TypeCastExpression { loc, .. }
            | InstructionValue::CallExpression { loc, .. }
            | InstructionValue::MethodCall { loc, .. }
            | InstructionValue::NewExpression { loc, .. }
            | InstructionValue::ObjectExpression { loc, .. }
            | InstructionValue::ObjectMethod { loc, .. }
            | InstructionValue::ArrayExpression { loc, .. }
            | InstructionValue::PropertyLoad { loc, .. }
            | InstructionValue::PropertyStore { loc, .. }
            | InstructionValue::PropertyDelete { loc, .. }
            | InstructionValue::ComputedLoad { loc, .. }
            | InstructionValue::ComputedStore { loc, .. }
            | InstructionValue::ComputedDelete { loc, .. }
            | InstructionValue::JsxExpression { loc, .. }
            | InstructionValue::JsxFragment { loc, .. }
            | InstructionValue::FunctionExpression { loc, .. }
            | InstructionValue::TemplateLiteral { loc, .. }
            | InstructionValue::TaggedTemplateExpression { loc, .. }
            | InstructionValue::Await { loc, .. }
            | InstructionValue::GetIterator { loc, .. }
            | InstructionValue::IteratorNext { loc, .. }
            | InstructionValue::NextPropertyOf { loc, .. }
            | InstructionValue::PrefixUpdate { loc, .. }
            | InstructionValue::PostfixUpdate { loc, .. }
            | InstructionValue::RegExpLiteral { loc, .. }
            | InstructionValue::MetaProperty { loc, .. }
            | InstructionValue::Debugger { loc }
            | InstructionValue::StartMemoize { loc, .. }
            | InstructionValue::FinishMemoize { loc, .. }
            | InstructionValue::InlineJs { loc, .. }
            | InstructionValue::UnsupportedNode { loc } => loc,
        }
    }
}

// ---------------------------------------------------------------------------
// Helper enums for InstructionValue
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PrimitiveValue {
    Number(f64),
    Boolean(bool),
    String(String),
    Null,
    Undefined,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum CallArg {
    Place(Place),
    Spread(SpreadPattern),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ObjectExpressionProperty {
    Property(ObjectProperty),
    Spread(SpreadPattern),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FunctionExpressionType {
    Arrow,
    Expression,
    Declaration,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextLValue {
    pub kind: ContextDeclKind,
    pub place: Place,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContextDeclKind {
    Let,
    HoistedConst,
    HoistedLet,
    HoistedFunction,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextStoreLValue {
    pub kind: ContextStoreKind,
    pub place: Place,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContextStoreKind {
    Reassign,
    Const,
    Let,
    Function,
}

// ---------------------------------------------------------------------------
// Instruction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Instruction {
    pub id: InstructionId,
    pub lvalue: Place,
    pub value: InstructionValue,
    pub loc: SourceLocation,
    pub effects: Option<Vec<AliasingEffect>>,
}

// ---------------------------------------------------------------------------
// Basic block
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BasicBlock {
    pub kind: BlockKind,
    pub id: BlockId,
    pub instructions: Vec<Instruction>,
    pub terminal: Terminal,
    pub preds: HashSet<BlockId>,
    pub phis: Vec<Phi>,
}

// ---------------------------------------------------------------------------
// HIR (control-flow graph)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HIR {
    pub entry: BlockId,
    /// Blocks in reverse-postorder (predecessors before successors).
    pub blocks: IndexMap<BlockId, BasicBlock>,
}

impl HIR {
    pub fn new(entry: BlockId) -> Self {
        HIR {
            entry,
            blocks: IndexMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Param
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Param {
    Place(Place),
    Spread(SpreadPattern),
}

// ---------------------------------------------------------------------------
// ReactFunctionType
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ReactFunctionType {
    Component,
    Hook,
    Other,
}

// ---------------------------------------------------------------------------
// HIRFunction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HIRFunction {
    pub loc: SourceLocation,
    pub id: Option<String>,
    pub name_hint: Option<String>,
    pub fn_type: ReactFunctionType,
    pub params: Vec<Param>,
    pub return_type_annotation: Option<TypeAnnotation>,
    pub returns: Place,
    pub context: Vec<Place>,
    pub body: HIR,
    pub generator: bool,
    pub async_: bool,
    pub directives: Vec<String>,
    pub aliasing_effects: Option<Vec<AliasingEffect>>,
    /// The original source re-emitted as clean JS (TS types stripped).
    /// Used as passthrough output while full codegen is not yet implemented.
    pub original_source: String,
    /// True if this function was originally declared as an arrow function
    /// (`const F = () => ...` or `() => ...`). Used in codegen to emit
    /// the correct output form: `const F = (params) => { ... }` vs `function F(params) { ... }`.
    pub is_arrow: bool,
    /// True if the function was declared with `export function` or `export const`.
    /// False for `export default function` (which uses a different keyword).
    pub is_named_export: bool,
    /// True if the function was declared with `export default function` or `export default () =>`.
    pub is_default_export: bool,
    /// The reactive function tree built by build_reactive_function.
    /// None until that pass runs; Some after.
    pub reactive_block: Option<ReactiveBlock>,
}

// ---------------------------------------------------------------------------
// ReactiveFunction and reactive types
// ---------------------------------------------------------------------------

pub type ReactiveBlock = Vec<ReactiveStatement>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ReactiveStatement {
    Instruction(ReactiveInstruction),
    Terminal(ReactiveTerminalStatement),
    Scope(ReactiveScopeBlock),
    PrunedScope(PrunedReactiveScopeBlock),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveInstruction {
    pub id: InstructionId,
    pub lvalue: Option<Place>,
    pub value: ReactiveValue,
    pub effects: Option<Vec<AliasingEffect>>,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ReactiveValue {
    Instruction(InstructionValue),
    Logical(ReactiveLogicalValue),
    Sequence(ReactiveSequenceValue),
    Ternary(ReactiveTernaryValue),
    OptionalCall(ReactiveOptionalCallValue),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveLogicalValue {
    pub operator: LogicalOperator,
    pub left: Box<ReactiveValue>,
    pub right: Box<ReactiveValue>,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveTernaryValue {
    pub test: Box<ReactiveValue>,
    pub consequent: Box<ReactiveValue>,
    pub alternate: Box<ReactiveValue>,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveSequenceValue {
    pub instructions: Vec<ReactiveInstruction>,
    pub id: InstructionId,
    pub value: Box<ReactiveValue>,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveOptionalCallValue {
    pub id: InstructionId,
    pub value: Box<ReactiveValue>,
    pub optional: bool,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveTerminalStatement {
    pub terminal: ReactiveTerminal,
    pub label: Option<ReactiveLabel>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveLabel {
    pub id: BlockId,
    pub implicit: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveScopeBlock {
    pub scope: ReactiveScope,
    pub instructions: ReactiveBlock,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrunedReactiveScopeBlock {
    pub scope: ReactiveScope,
    pub instructions: ReactiveBlock,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ReactiveTerminal {
    Break { target: BlockId, id: InstructionId, target_kind: ReactiveTerminalTargetKind, loc: SourceLocation },
    Continue { target: BlockId, id: InstructionId, target_kind: ReactiveTerminalTargetKind, loc: SourceLocation },
    Return { value: Place, id: InstructionId, loc: SourceLocation },
    Throw { value: Place, id: InstructionId, loc: SourceLocation },
    Switch {
        test: Place,
        cases: Vec<ReactiveSwitchCase>,
        id: InstructionId,
        loc: SourceLocation,
    },
    DoWhile { loop_: ReactiveBlock, test: Box<ReactiveValue>, id: InstructionId, loc: SourceLocation, test_bid: BlockId },
    While { test: Box<ReactiveValue>, loop_: ReactiveBlock, id: InstructionId, loc: SourceLocation, test_bid: BlockId },
    For {
        init: Box<ReactiveValue>,
        test: Box<ReactiveValue>,
        update: Option<Box<ReactiveValue>>,
        loop_: ReactiveBlock,
        id: InstructionId,
        loc: SourceLocation,
        /// Original HIR block IDs for init/test/update — used by tree codegen
        /// to emit the for-loop header using the same approach as flat codegen.
        init_bid: BlockId,
        test_bid: BlockId,
        update_bid: Option<BlockId>,
    },
    ForOf {
        loop_var: String,
        iterable: Box<ReactiveValue>,
        loop_: ReactiveBlock,
        id: InstructionId,
        loc: SourceLocation,
        iterable_bid: BlockId,
    },
    ForIn {
        loop_var: String,
        object: Box<ReactiveValue>,
        loop_: ReactiveBlock,
        id: InstructionId,
        loc: SourceLocation,
        object_bid: BlockId,
    },
    If { test: Place, consequent: ReactiveBlock, alternate: Option<ReactiveBlock>, id: InstructionId, loc: SourceLocation },
    Label { block: ReactiveBlock, id: InstructionId, loc: SourceLocation },
    Try { block: ReactiveBlock, handler_binding: Option<Place>, handler: ReactiveBlock, id: InstructionId, loc: SourceLocation },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReactiveTerminalTargetKind {
    Implicit,
    Labeled,
    Unlabeled,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveSwitchCase {
    pub test: Option<Place>,
    pub block: Option<ReactiveBlock>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactiveFunction {
    pub loc: SourceLocation,
    pub id: Option<String>,
    pub name_hint: Option<String>,
    pub params: Vec<Param>,
    pub generator: bool,
    pub async_: bool,
    pub body: ReactiveBlock,
    pub directives: Vec<String>,
}
