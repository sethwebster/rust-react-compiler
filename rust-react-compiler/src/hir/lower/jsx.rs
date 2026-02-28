#![allow(unused_imports, unused_variables, dead_code)]
use oxc_ast::ast::*;
use oxc_semantic::Semantic;
use crate::hir::hir::{self, *};
use crate::error::{CompilerError, Result};
use super::LoweringContext;

// ---------------------------------------------------------------------------
// lower_jsx_element
//
// Lowers a JSXElement into a HIR JsxExpression instruction.
//
// Steps:
//   1. Lower the tag from the opening element name.
//   2. Lower all JSX attributes into Vec<JsxAttribute>.
//   3. Lower all JSX children into Vec<Place>.
//   4. Emit JsxExpression.
// ---------------------------------------------------------------------------

pub fn lower_jsx_element<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    elem: &JSXElement<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let elem_loc = SourceLocation::source(elem.span.start, elem.span.end);
    let opening = &elem.opening_element;
    let opening_loc = SourceLocation::source(opening.span.start, opening.span.end);

    // Step 1: Lower the tag.
    let tag = lower_jsx_tag(ctx, semantic, &opening.name, lower_expr)?;

    // Step 2: Lower attributes.
    let mut props: Vec<JsxAttribute> = Vec::with_capacity(opening.attributes.len());
    for attr_item in &opening.attributes {
        match attr_item {
            JSXAttributeItem::Attribute(attr) => {
                let attr_loc = SourceLocation::source(attr.span.start, attr.span.end);

                let name = match &attr.name {
                    JSXAttributeName::Identifier(id) => id.name.to_string(),
                    JSXAttributeName::NamespacedName(ns) => {
                        format!("{}:{}", ns.namespace.name, ns.name.name)
                    }
                };

                let place = match &attr.value {
                    None => {
                        // Bare attribute like `disabled` — lower as `true`.
                        ctx.push(
                            InstructionValue::Primitive {
                                value: PrimitiveValue::Boolean(true),
                                loc: attr_loc.clone(),
                            },
                            attr_loc,
                        )
                    }
                    Some(JSXAttributeValue::StringLiteral(s)) => {
                        let str_loc = SourceLocation::source(s.span.start, s.span.end);
                        ctx.push(
                            InstructionValue::Primitive {
                                value: PrimitiveValue::String(s.value.to_string()),
                                loc: str_loc.clone(),
                            },
                            str_loc,
                        )
                    }
                    Some(JSXAttributeValue::ExpressionContainer(c)) => {
                        lower_jsx_expression_container(ctx, semantic, c, lower_expr)?
                            .ok_or_else(|| {
                                CompilerError::todo(
                                    "JSX attribute with empty expression container",
                                )
                            })?
                    }
                    Some(JSXAttributeValue::Element(el)) => {
                        lower_jsx_element(ctx, semantic, el, lower_expr)?
                    }
                    Some(JSXAttributeValue::Fragment(frag)) => {
                        lower_jsx_fragment(ctx, semantic, frag, lower_expr)?
                    }
                };

                props.push(JsxAttribute::Attribute { name, place });
            }
            JSXAttributeItem::SpreadAttribute(spread) => {
                let argument = lower_expr(&spread.argument, ctx)?;
                props.push(JsxAttribute::Spread { argument });
            }
        }
    }

    // Step 3: Lower children.
    let mut children_places: Vec<Place> = Vec::new();
    for child in &elem.children {
        lower_jsx_child(ctx, semantic, child, lower_expr, &mut children_places)?;
    }

    // Step 4: Emit JsxExpression.
    //
    // The TypeScript compiler sets children to None only for self-closing
    // elements that have no children. We mirror that here.
    let is_self_closing = elem.closing_element.is_none();
    let children = if is_self_closing && children_places.is_empty() {
        None
    } else {
        Some(children_places)
    };

    let closing_loc = elem
        .closing_element
        .as_ref()
        .map(|c| SourceLocation::source(c.span.start, c.span.end))
        .unwrap_or(SourceLocation::Generated);

    Ok(ctx.push(
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            loc: elem_loc.clone(),
            opening_loc,
            closing_loc,
        },
        elem_loc,
    ))
}

// ---------------------------------------------------------------------------
// lower_jsx_fragment
//
// Lowers a JSXFragment into a HIR JsxFragment instruction.
// ---------------------------------------------------------------------------

pub fn lower_jsx_fragment<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    frag: &JSXFragment<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(frag.span.start, frag.span.end);

    let mut children_places: Vec<Place> = Vec::new();
    for child in &frag.children {
        lower_jsx_child(ctx, semantic, child, lower_expr, &mut children_places)?;
    }

    Ok(ctx.push(
        InstructionValue::JsxFragment {
            children: children_places,
            loc: loc.clone(),
        },
        loc,
    ))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Lower a JSXElementName into a JsxTag.
///
/// Uppercase-starting or dotted identifiers → component → LoadGlobal → JsxTag::Place.
/// Lowercase identifiers → intrinsic → JsxTag::Builtin.
fn lower_jsx_tag<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    name: &JSXElementName<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<JsxTag> {
    match name {
        JSXElementName::Identifier(ident) => {
            let name_str = ident.name.as_str();
            let loc = SourceLocation::source(ident.span.start, ident.span.end);

            // Convention: uppercase first char or name containing '.' → component.
            // (Dots in a plain identifier can't occur here; MemberExpression handles that.
            // We check only for uppercase to distinguish <Foo> from <div>.)
            let first_char = name_str.chars().next().unwrap_or('a');
            if first_char.is_uppercase() {
                // Component reference — load from scope or treat as global.
                let place = ctx.push(
                    InstructionValue::LoadGlobal {
                        binding: NonLocalBinding::Global {
                            name: name_str.to_string(),
                        },
                        loc: loc.clone(),
                    },
                    loc,
                );
                Ok(JsxTag::Place(place))
            } else {
                // Intrinsic / HTML element.
                Ok(JsxTag::Builtin(BuiltinTag {
                    name: name_str.to_string(),
                    loc,
                }))
            }
        }

        JSXElementName::MemberExpression(member) => {
            // e.g. `Foo.Bar` or `A.B.C` — always a component reference.
            let loc = SourceLocation::source(member.span.start, member.span.end);
            let name_str = jsx_member_expr_to_string(member);
            let place = ctx.push(
                InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global { name: name_str },
                    loc: loc.clone(),
                },
                loc,
            );
            Ok(JsxTag::Place(place))
        }

        JSXElementName::NamespacedName(ns) => {
            // e.g. `foo:bar` — treat as an intrinsic with the full qualified name.
            let loc = SourceLocation::source(ns.span.start, ns.span.end);
            Ok(JsxTag::Builtin(BuiltinTag {
                name: format!("{}:{}", ns.namespace.name, ns.name.name),
                loc,
            }))
        }

        JSXElementName::IdentifierReference(ident) => {
            // e.g. <Component /> where Component is an identifier reference.
            let loc = SourceLocation::source(ident.span.start, ident.span.end);
            let place = ctx.push(
                InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global {
                        name: ident.name.to_string(),
                    },
                    loc: loc.clone(),
                },
                loc,
            );
            Ok(JsxTag::Place(place))
        }

        JSXElementName::ThisExpression(this) => {
            // e.g. <this.Foo /> — treat as a "this" load.
            let loc = SourceLocation::source(this.span.start, this.span.end);
            let place = ctx.push(
                InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global { name: "this".to_string() },
                    loc: loc.clone(),
                },
                loc,
            );
            Ok(JsxTag::Place(place))
        }
    }
}

/// Recursively flatten a JSXMemberExpression into a dotted string like "Foo.Bar.Baz".
///
/// Note: `JSXMemberExpressionObject` in oxc 0.69 has variants `IdentifierReference` and
/// `MemberExpression`. If the crate version adds `ThisExpression` we handle it too.
fn jsx_member_expr_to_string(member: &JSXMemberExpression) -> String {
    let object_str = match &member.object {
        JSXMemberExpressionObject::IdentifierReference(id) => id.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(nested) => {
            jsx_member_expr_to_string(nested)
        }
        JSXMemberExpressionObject::ThisExpression(_) => "this".to_string(),
    };
    format!("{}.{}", object_str, member.property.name)
}

/// Lower one JSXChild, appending the resulting Place (if any) to `out`.
fn lower_jsx_child<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    child: &JSXChild<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
    out: &mut Vec<Place>,
) -> Result<()> {
    match child {
        JSXChild::Text(text) => {
            // Trim surrounding whitespace-only text nodes, matching React compiler behaviour.
            let trimmed = trim_jsx_text(text.value.as_str());
            if !trimmed.is_empty() {
                let loc = SourceLocation::source(text.span.start, text.span.end);
                let place = ctx.push(
                    InstructionValue::JsxText {
                        value: trimmed,
                        loc: loc.clone(),
                    },
                    loc,
                );
                out.push(place);
            }
        }

        JSXChild::ExpressionContainer(c) => {
            if let Some(place) =
                lower_jsx_expression_container(ctx, semantic, c, lower_expr)?
            {
                out.push(place);
            }
        }

        JSXChild::Element(el) => {
            let place = lower_jsx_element(ctx, semantic, el, lower_expr)?;
            out.push(place);
        }

        JSXChild::Fragment(frag) => {
            let place = lower_jsx_fragment(ctx, semantic, frag, lower_expr)?;
            out.push(place);
        }

        JSXChild::Spread(spread) => {
            let place = lower_expr(&spread.expression, ctx)?;
            out.push(place);
        }
    }
    Ok(())
}

/// Lower a JSXExpressionContainer.
///
/// Returns `None` for empty expression containers (`{}`), which are legal in
/// JSX children but produce no runtime value.
fn lower_jsx_expression_container<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    container: &JSXExpressionContainer<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Option<Place>> {
    match &container.expression {
        JSXExpression::EmptyExpression(_) => Ok(None),
        expr => {
            // All non-empty JSXExpression variants inherit from Expression via
            // oxc's ast_macros. `as_expression()` returns Some(&Expression<'a>)
            // for every variant except EmptyExpression.
            let inner = expr.as_expression().ok_or_else(|| {
                CompilerError::invariant("JSXExpression::as_expression() returned None for non-empty variant")
            })?;
            let place = lower_expr(inner, ctx)?;
            Ok(Some(place))
        }
    }
}

/// Trim JSX text content following the JSX spec:
///   > JSX removes whitespace at the beginning and ending of a line.
///   > It also removes blank lines. New lines adjacent to tags are removed;
///   > new lines that occur in the middle of string literals are condensed
///   > into a single space.
///
/// Faithfully ported from `trimJsxText` in BuildHIR.ts (itself adapted from Babel).
///
/// Returns an empty string when the entire node should be dropped.
fn trim_jsx_text(original: &str) -> String {
    // Split on \r\n, \n, or \r (matching the TS regex /\r\n|\n|\r/).
    let lines: Vec<&str> = split_newlines(original);

    // Find the last line that has any non-space/tab content.
    let mut last_non_empty_line = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if line.contains(|c: char| c != ' ' && c != '\t') {
            last_non_empty_line = i;
        }
    }

    let mut result = String::new();

    for (i, line) in lines.iter().enumerate() {
        let is_first_line = i == 0;
        let is_last_line = i == lines.len() - 1;
        let is_last_non_empty_line = i == last_non_empty_line;

        // Replace rendered tab characters with spaces.
        let mut trimmed = line.replace('\t', " ");

        // Trim whitespace touching a preceding newline (all lines after the first).
        if !is_first_line {
            trimmed = trimmed.trim_start_matches(' ').to_string();
        }

        // Trim whitespace touching a following newline (all lines before the last).
        if !is_last_line {
            trimmed = trimmed.trim_end_matches(' ').to_string();
        }

        if !trimmed.is_empty() {
            if !is_last_non_empty_line {
                trimmed.push(' ');
            }
            result.push_str(&trimmed);
        }
    }

    result
}

/// Split a string on \r\n, \n, or \r — equivalent to JS `.split(/\r\n|\n|\r/)`.
fn split_newlines(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            result.push(&s[start..i]);
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
            start = i;
        } else if bytes[i] == b'\n' {
            result.push(&s[start..i]);
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    result.push(&s[start..]);
    result
}
