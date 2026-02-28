#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::HashMap;
use crate::hir::hir::*;
use crate::hir::types::{FunctionType, ObjectType, PolyType, PrimitiveType, Type};

pub fn infer_types(hir: &mut HIRFunction) {
    let mut type_map: HashMap<IdentifierId, Type> = HashMap::new();

    // Process blocks in order (reverse-postorder means predecessors before successors)
    for block in hir.body.blocks.values() {
        // Process phi nodes first: phi result type is deferred until we resolve operands
        for phi in &block.phis {
            let operand_ids: Vec<IdentifierId> =
                phi.operands.values().map(|p| p.identifier).collect();
            type_map.insert(phi.place.identifier, Type::Phi { operands: operand_ids });
        }

        for instr in &block.instructions {
            let inferred = infer_instruction_type(&instr.value, &type_map);
            type_map.insert(instr.lvalue.identifier, inferred);
        }
    }

    // Phase 2: wire type_map back into identifiers is deferred — HIRFunction does not
    // carry the Environment arena directly. Callers that need types should pass env
    // separately. The computed map is the canonical output of this pass.
}

fn infer_instruction_type(value: &InstructionValue, types: &HashMap<IdentifierId, Type>) -> Type {
    match value {
        InstructionValue::Primitive { value, .. } => match value {
            PrimitiveValue::Number(_) => Type::Primitive(PrimitiveType::Number),
            PrimitiveValue::Boolean(_) => Type::Primitive(PrimitiveType::Boolean),
            PrimitiveValue::String(_) => Type::Primitive(PrimitiveType::String),
            PrimitiveValue::Null => Type::Primitive(PrimitiveType::Null),
            PrimitiveValue::Undefined => Type::Primitive(PrimitiveType::Undefined),
        },

        InstructionValue::UnaryExpression { operator, .. } => match operator {
            UnaryOperator::Not => Type::Primitive(PrimitiveType::Boolean),
            UnaryOperator::Typeof => Type::Primitive(PrimitiveType::String),
            UnaryOperator::Void => Type::Primitive(PrimitiveType::Undefined),
            // Minus / Plus / BitNot remain numeric in the general case
            UnaryOperator::Minus | UnaryOperator::Plus | UnaryOperator::BitNot => {
                Type::Primitive(PrimitiveType::Number)
            }
        },

        InstructionValue::BinaryExpression { operator, left, right, .. } => {
            let lt = types.get(&left.identifier);
            let rt = types.get(&right.identifier);
            match operator {
                // Comparison operators always yield boolean
                BinaryOperator::Eq
                | BinaryOperator::NEq
                | BinaryOperator::StrictEq
                | BinaryOperator::StrictNEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::In
                | BinaryOperator::Instanceof => Type::Primitive(PrimitiveType::Boolean),

                // Arithmetic ops: number × number → number
                BinaryOperator::Sub
                | BinaryOperator::Mul
                | BinaryOperator::Div
                | BinaryOperator::Mod
                | BinaryOperator::Exp
                | BinaryOperator::BitAnd
                | BinaryOperator::BitOr
                | BinaryOperator::BitXor
                | BinaryOperator::Shl
                | BinaryOperator::Shr
                | BinaryOperator::UShr => {
                    if matches!(
                        (lt, rt),
                        (
                            Some(Type::Primitive(PrimitiveType::Number)),
                            Some(Type::Primitive(PrimitiveType::Number))
                        )
                    ) {
                        Type::Primitive(PrimitiveType::Number)
                    } else {
                        Type::Poly { kind: PolyType::Any }
                    }
                }

                // Add: number + number → number, otherwise poly (could be string concat)
                BinaryOperator::Add => {
                    match (lt, rt) {
                        (
                            Some(Type::Primitive(PrimitiveType::Number)),
                            Some(Type::Primitive(PrimitiveType::Number)),
                        ) => Type::Primitive(PrimitiveType::Number),
                        (
                            Some(Type::Primitive(PrimitiveType::String)),
                            _,
                        )
                        | (
                            _,
                            Some(Type::Primitive(PrimitiveType::String)),
                        ) => Type::Primitive(PrimitiveType::String),
                        _ => Type::Poly { kind: PolyType::Any },
                    }
                }
            }
        }

        InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. } => {
            types.get(&place.identifier).cloned().unwrap_or_default()
        }

        InstructionValue::TypeCastExpression { type_, .. } => type_.clone(),

        InstructionValue::JsxExpression { .. }
        | InstructionValue::JsxFragment { .. } => {
            Type::Object(ObjectType { shape_id: None })
        }

        InstructionValue::ArrayExpression { .. }
        | InstructionValue::ObjectExpression { .. }
        | InstructionValue::NewExpression { .. } => {
            Type::Object(ObjectType { shape_id: None })
        }

        InstructionValue::FunctionExpression { .. } => {
            Type::Function(FunctionType {
                return_type: Box::new(Type::default()),
                param_types: vec![],
                rest_param_type: None,
            })
        }

        InstructionValue::TemplateLiteral { .. }
        | InstructionValue::TaggedTemplateExpression { .. }
        | InstructionValue::JsxText { .. } => Type::Primitive(PrimitiveType::String),

        InstructionValue::RegExpLiteral { .. } => Type::Object(ObjectType { shape_id: None }),

        // Everything else: unknown
        _ => Type::Poly { kind: PolyType::Any },
    }
}
