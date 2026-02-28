use crate::hir::hir::IdentifierId;

/// The type of a value as inferred by InferTypes pass.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Type {
    /// Type is unknown/not yet inferred
    Poly { kind: PolyType },
    /// Primitive JS types
    Primitive(PrimitiveType),
    /// A function type
    Function(FunctionType),
    /// An object/reference type
    Object(ObjectType),
    /// Phi (join) type — result of merging types at control flow join point
    Phi { operands: Vec<IdentifierId> },
}

impl Default for Type {
    fn default() -> Self {
        Type::Poly { kind: PolyType::Any }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PolyType {
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PrimitiveType {
    Number,
    Boolean,
    String,
    Null,
    Undefined,
    Symbol,
    BigInt,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FunctionType {
    pub return_type: Box<Type>,
    pub param_types: Vec<Type>,
    pub rest_param_type: Option<Box<Type>>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ObjectType {
    pub shape_id: Option<ShapeId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ShapeId(pub u32);

pub fn make_type() -> Type {
    Type::default()
}
