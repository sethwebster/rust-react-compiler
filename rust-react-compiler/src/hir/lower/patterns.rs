#![allow(unused_imports, unused_variables, dead_code)]
use oxc_ast::ast::*;
use oxc_index::Idx;
use oxc_semantic::Semantic;
use oxc_span::GetSpan;
use crate::hir::hir::{
    ArrayPattern as HirArrayPattern,
    ObjectPattern as HirObjectPattern,
    ObjectProperty as HirObjectProperty,
    SourceLocation, Place, InstructionValue, InstructionKind,
    LValue, LValuePattern, Pattern,
    ArrayElement, SpreadPattern,
    ObjectPatternProperty, ObjectPropertyKey, ObjectPropertyType,
};
use crate::error::{CompilerError, Result};
use super::LoweringContext;

// ---------------------------------------------------------------------------
// lower_binding_pattern
//
// Bind the Place `value` to the binding described by `pattern`, emitting the
// appropriate HIR instruction(s) according to `kind` (Const / Let / Reassign).
//
// - BindingIdentifier → StoreLocal
// - ArrayPattern      → Destructure (array)
// - ObjectPattern     → Destructure (object)
// - AssignmentPattern → simplified: lower the left side (default ignored)
// ---------------------------------------------------------------------------

pub fn lower_binding_pattern<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    pattern: &BindingPattern<'a>,
    value: Place,
    kind: InstructionKind,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    let loc = SourceLocation::source(pattern.span().start, pattern.span().end);

    match &pattern.kind {
        BindingPatternKind::BindingIdentifier(ident) => {
            let maybe_sym = ident.symbol_id.get();
            let id = if let Some(sym_id) = maybe_sym {
                ctx.get_or_create_symbol(sym_id.index() as u32, Some(ident.name.as_str()), loc.clone())
            } else {
                ctx.env.new_temporary(loc.clone())
            };
            let lvalue = LValue { place: Place::new(id, loc.clone()), kind };
            ctx.push(
                InstructionValue::StoreLocal {
                    lvalue,
                    value,
                    type_annotation: None,
                    loc: loc.clone(),
                },
                loc,
            );
        }

        BindingPatternKind::ArrayPattern(ap) => {
            lower_array_pattern(ctx, semantic, ap, value, kind, lower_expr)?;
        }

        BindingPatternKind::ObjectPattern(op) => {
            lower_object_pattern(ctx, semantic, op, value, kind, lower_expr)?;
        }

        BindingPatternKind::AssignmentPattern(ap) => {
            // Simplified: lower the left binding with the incoming value,
            // ignoring the default expression.  A full implementation would
            // test for undefined at runtime and substitute the default.
            lower_binding_pattern(ctx, semantic, &ap.left, value, kind, lower_expr)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// lower_array_pattern
//
// Emit a Destructure instruction for an array destructuring pattern.
//
// Pattern elements map to ArrayElement variants:
//   None                        → Hole
//   Some(BindingIdentifier)     → Place (resolved symbol or temporary)
//   Some(other pattern)         → Place (temporary; nested pattern lowered
//                                        afterward)
//   rest element                → Spread
// ---------------------------------------------------------------------------

pub fn lower_array_pattern<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    pattern: &oxc_ast::ast::ArrayPattern<'a>,
    value: Place,
    kind: InstructionKind,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    let loc = SourceLocation::source(pattern.span.start, pattern.span.end);

    // Collect places (and any nested patterns that need post-processing).
    let mut items: Vec<ArrayElement> = Vec::new();
    // Track (place, binding_pattern) pairs for nested patterns that need a
    // subsequent lower_binding_pattern call after the Destructure is emitted.
    let mut nested: Vec<(Place, &BindingPattern<'a>)> = Vec::new();

    for elem in &pattern.elements {
        match elem {
            None => {
                items.push(ArrayElement::Hole);
            }
            Some(elem_pat) => {
                match &elem_pat.kind {
                    BindingPatternKind::BindingIdentifier(ident) => {
                        let maybe_sym = ident.symbol_id.get();
                        let id = if let Some(sym_id) = maybe_sym {
                            ctx.get_or_create_symbol(
                                sym_id.index() as u32,
                                Some(ident.name.as_str()),
                                loc.clone(),
                            )
                        } else {
                            ctx.env.new_temporary(loc.clone())
                        };
                        items.push(ArrayElement::Place(Place::new(id, loc.clone())));
                    }
                    _ => {
                        // Nested pattern: bind to a temporary, lower afterward.
                        let tmp = ctx.make_temporary(loc.clone());
                        nested.push((tmp.clone(), elem_pat));
                        items.push(ArrayElement::Place(tmp));
                    }
                }
            }
        }
    }

    // Handle the rest element (if present).
    if let Some(rest) = &pattern.rest {
        let inner = &rest.argument;
        match &inner.kind {
            BindingPatternKind::BindingIdentifier(ident) => {
                let maybe_sym = ident.symbol_id.get();
                let id = if let Some(sym_id) = maybe_sym {
                    ctx.get_or_create_symbol(
                        sym_id.index() as u32,
                        Some(ident.name.as_str()),
                        loc.clone(),
                    )
                } else {
                    ctx.env.new_temporary(loc.clone())
                };
                items.push(ArrayElement::Spread(SpreadPattern {
                    place: Place::new(id, loc.clone()),
                }));
            }
            _ => {
                let tmp = ctx.make_temporary(loc.clone());
                nested.push((tmp.clone(), inner));
                items.push(ArrayElement::Spread(SpreadPattern { place: tmp }));
            }
        }
    }

    let hir_pattern = Pattern::Array(HirArrayPattern { items, loc: loc.clone() });
    ctx.push(
        InstructionValue::Destructure {
            lvalue: LValuePattern { pattern: hir_pattern, kind },
            value,
            loc: loc.clone(),
        },
        loc,
    );

    // Lower any nested patterns using their respective temporaries.
    for (tmp_place, nested_pat) in nested {
        lower_binding_pattern(ctx, semantic, nested_pat, tmp_place, kind, lower_expr)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// lower_object_pattern
//
// Emit a Destructure instruction for an object destructuring pattern.
//
// For each BindingProperty:
//   - Resolve the destination place (identifier → resolved symbol; nested
//     pattern → temporary, lowered afterward).
//   - Resolve the key (StaticIdentifier / StringLiteral / NumericLiteral /
//     computed expression).
//
// A BindingRestElement produces a Spread property.
// ---------------------------------------------------------------------------

pub fn lower_object_pattern<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    pattern: &oxc_ast::ast::ObjectPattern<'a>,
    value: Place,
    kind: InstructionKind,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    let loc = SourceLocation::source(pattern.span.start, pattern.span.end);

    let mut properties: Vec<ObjectPatternProperty> = Vec::new();
    let mut nested: Vec<(Place, &BindingPattern<'a>)> = Vec::new();

    for prop in &pattern.properties {
        // Resolve the destination place.
        let (dest_place, is_nested) = match &prop.value.kind {
            BindingPatternKind::BindingIdentifier(ident) => {
                let maybe_sym = ident.symbol_id.get();
                let id = if let Some(sym_id) = maybe_sym {
                    ctx.get_or_create_symbol(
                        sym_id.index() as u32,
                        Some(ident.name.as_str()),
                        loc.clone(),
                    )
                } else {
                    ctx.env.new_temporary(loc.clone())
                };
                (Place::new(id, loc.clone()), false)
            }
            _ => {
                let tmp = ctx.make_temporary(loc.clone());
                (tmp, true)
            }
        };

        if is_nested {
            nested.push((dest_place.clone(), &prop.value));
        }

        // Resolve the property key.
        let key = lower_property_key(ctx, &prop.key, loc.clone(), lower_expr)?;

        properties.push(ObjectPatternProperty::Property(HirObjectProperty {
            key,
            type_: ObjectPropertyType::Property,
            place: dest_place,
        }));
    }

    // Handle the rest element (if present).
    if let Some(rest) = &pattern.rest {
        let inner = &rest.argument;
        match &inner.kind {
            BindingPatternKind::BindingIdentifier(ident) => {
                let maybe_sym = ident.symbol_id.get();
                let id = if let Some(sym_id) = maybe_sym {
                    ctx.get_or_create_symbol(
                        sym_id.index() as u32,
                        Some(ident.name.as_str()),
                        loc.clone(),
                    )
                } else {
                    ctx.env.new_temporary(loc.clone())
                };
                properties.push(ObjectPatternProperty::Spread(SpreadPattern {
                    place: Place::new(id, loc.clone()),
                }));
            }
            _ => {
                let tmp = ctx.make_temporary(loc.clone());
                nested.push((tmp.clone(), inner));
                properties.push(ObjectPatternProperty::Spread(SpreadPattern { place: tmp }));
            }
        }
    }

    let hir_pattern = Pattern::Object(HirObjectPattern { properties, loc: loc.clone() });
    ctx.push(
        InstructionValue::Destructure {
            lvalue: LValuePattern { pattern: hir_pattern, kind },
            value,
            loc: loc.clone(),
        },
        loc,
    );

    // Lower any nested patterns.
    for (tmp_place, nested_pat) in nested {
        lower_binding_pattern(ctx, semantic, nested_pat, tmp_place, kind, lower_expr)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Convert an oxc PropertyKey (from a BindingProperty) to our ObjectPropertyKey.
/// For computed keys we emit a temporary and lower the expression.
fn lower_property_key<'a>(
    ctx: &mut LoweringContext,
    key: &PropertyKey<'a>,
    loc: SourceLocation,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<ObjectPropertyKey> {
    match key {
        PropertyKey::StaticIdentifier(ident) => {
            Ok(ObjectPropertyKey::Identifier(ident.name.to_string()))
        }
        PropertyKey::StringLiteral(s) => {
            Ok(ObjectPropertyKey::String(s.value.to_string()))
        }
        PropertyKey::NumericLiteral(n) => {
            Ok(ObjectPropertyKey::Number(n.value))
        }
        _ => {
            // Computed/expression key (including PrivateIdentifier, BigInt,
            // template literals, etc.) — attempt to lower via as_expression().
            if let Some(expr) = key.as_expression() {
                let key_place = lower_expr(expr, ctx)?;
                Ok(ObjectPropertyKey::Computed(key_place))
            } else {
                // Private identifiers and other non-expression keys: use a
                // temporary as a placeholder.
                let tmp = ctx.make_temporary(loc);
                Ok(ObjectPropertyKey::Computed(tmp))
            }
        }
    }
}
