/// Debug printing for HIR — mirrors printHIRFunction in the TS compiler.
use std::fmt::Write;
use crate::hir::hir::*;
use crate::hir::environment::Environment;

pub fn print_hir_function(func: &HIRFunction, env: &Environment) -> String {
    let mut out = String::new();

    let name = func.id.as_deref().unwrap_or("<anonymous>");
    let async_ = if func.async_ { "async " } else { "" };
    let gen = if func.generator { "*" } else { "" };
    let _ = writeln!(out, "{}function{} {}(", async_, gen, name);

    for param in &func.params {
        match param {
            Param::Place(p) => {
                let _ = writeln!(out, "  {},", print_place(p, env));
            }
            Param::Spread(s) => {
                let _ = writeln!(out, "  ...{},", print_place(&s.place, env));
            }
        }
    }
    let _ = writeln!(out, "): {}", print_place(&func.returns, env));
    let _ = writeln!(out);

    print_hir(&func.body, env, &mut out);
    out
}

fn print_hir(hir: &HIR, env: &Environment, out: &mut String) {
    for (block_id, block) in &hir.blocks {
        let kind = match block.kind {
            BlockKind::Block => "block",
            BlockKind::Value => "value",
            BlockKind::Loop => "loop",
            BlockKind::Sequence => "sequence",
            BlockKind::Catch => "catch",
        };
        let _ = writeln!(out, "bb{}({}):", block_id.0, kind);

        if !block.phis.is_empty() {
            for phi in &block.phis {
                let operands: Vec<String> = phi
                    .operands
                    .iter()
                    .map(|(bid, p)| format!("bb{}:{}", bid.0, print_place(p, env)))
                    .collect();
                let _ = writeln!(
                    out,
                    "  {} = φ({})",
                    print_place(&phi.place, env),
                    operands.join(", ")
                );
            }
        }

        for instr in &block.instructions {
            let _ = writeln!(
                out,
                "  [{}] {} = {}",
                instr.id.0,
                print_place(&instr.lvalue, env),
                print_instruction_value(&instr.value, env),
            );
        }

        let _ = writeln!(out, "  → {}", print_terminal(&block.terminal));
        let _ = writeln!(out);
    }
}

fn print_place(place: &Place, env: &Environment) -> String {
    let name = env
        .get_identifier(place.identifier)
        .and_then(|i| i.name.as_ref())
        .map(|n| n.value().to_string())
        .unwrap_or_else(|| format!("$t{}", place.identifier.0));
    format!("{}{}", name, print_effect(place.effect))
}

fn print_effect(effect: Effect) -> &'static str {
    match effect {
        Effect::Unknown => "",
        Effect::Read => "<read>",
        Effect::Freeze => "<freeze>",
        Effect::Capture => "<capture>",
        Effect::ConditionallyMutate => "<mutate?>",
        Effect::ConditionallyMutateIterator => "<mutate-iter?>",
        Effect::Mutate => "<mutate>",
        Effect::Store => "<store>",
    }
}

fn print_instruction_value(val: &InstructionValue, env: &Environment) -> String {
    match val {
        InstructionValue::Primitive { value, .. } => match value {
            PrimitiveValue::Undefined => "undefined".into(),
            PrimitiveValue::Null => "null".into(),
            PrimitiveValue::Boolean(b) => b.to_string(),
            PrimitiveValue::Number(n) => n.to_string(),
            PrimitiveValue::String(s) => format!("{:?}", s),
        },
        InstructionValue::LoadLocal { place, .. } => {
            format!("LoadLocal {}", print_place(place, env))
        }
        InstructionValue::LoadContext { place, .. } => {
            format!("LoadContext {}", print_place(place, env))
        }
        InstructionValue::LoadGlobal { binding, .. } => {
            format!("LoadGlobal {}", print_binding(binding))
        }
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            format!(
                "StoreLocal {} = {}",
                print_place(&lvalue.place, env),
                print_place(value, env)
            )
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            let args_str: Vec<_> = args.iter().map(|a| print_call_arg(a, env)).collect();
            format!("{}({})", print_place(callee, env), args_str.join(", "))
        }
        InstructionValue::MethodCall { receiver, property, args, .. } => {
            let args_str: Vec<_> = args.iter().map(|a| print_call_arg(a, env)).collect();
            format!(
                "{}.{}({})",
                print_place(receiver, env),
                print_place(property, env),
                args_str.join(", ")
            )
        }
        InstructionValue::PropertyLoad { object, property, .. } => {
            format!("{}.{}", print_place(object, env), property)
        }
        InstructionValue::PropertyStore { object, property, value, .. } => {
            format!(
                "{}.{} = {}",
                print_place(object, env),
                property,
                print_place(value, env)
            )
        }
        InstructionValue::BinaryExpression { operator, left, right, .. } => {
            format!(
                "{} {:?} {}",
                print_place(left, env),
                operator,
                print_place(right, env)
            )
        }
        InstructionValue::FunctionExpression { name, fn_type, .. } => {
            let n = name.as_deref().unwrap_or("<anonymous>");
            format!("{:?} {}", fn_type, n)
        }
        InstructionValue::JsxExpression { tag, .. } => {
            let tag_str = match tag {
                JsxTag::Builtin(b) => b.name.clone(),
                JsxTag::Place(p) => print_place(p, env),
            };
            format!("<{} ... />", tag_str)
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            let elems: Vec<_> = elements.iter().map(|e| match e {
                ArrayElement::Place(p) => print_place(p, env),
                ArrayElement::Spread(s) => format!("...{}", print_place(&s.place, env)),
                ArrayElement::Hole => "hole".into(),
            }).collect();
            format!("[{}]", elems.join(", "))
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            let props: Vec<_> = properties.iter().map(|p| match p {
                ObjectExpressionProperty::Property(op) => {
                    format!("{:?}: {}", op.key, print_place(&op.place, env))
                }
                ObjectExpressionProperty::Spread(s) => {
                    format!("...{}", print_place(&s.place, env))
                }
            }).collect();
            format!("{{{}}}", props.join(", "))
        }
        _ => format!("{:?}", std::mem::discriminant(val)),
    }
}

fn print_call_arg(arg: &CallArg, env: &Environment) -> String {
    match arg {
        CallArg::Place(p) => print_place(p, env),
        CallArg::Spread(s) => format!("...{}", print_place(&s.place, env)),
    }
}

fn print_binding(binding: &NonLocalBinding) -> String {
    match binding {
        NonLocalBinding::Global { name } => name.clone(),
        NonLocalBinding::ModuleLocal { name } => name.clone(),
        NonLocalBinding::ImportDefault { name, module } => format!("{} from '{}'", name, module),
        NonLocalBinding::ImportNamespace { name, module } => {
            format!("* as {} from '{}'", name, module)
        }
        NonLocalBinding::ImportSpecifier { name, module, imported } => {
            format!("{{ {} as {} }} from '{}'", imported, name, module)
        }
    }
}

fn print_terminal(terminal: &Terminal) -> String {
    match terminal {
        Terminal::Return { value, .. } => {
            format!("return $t{}", value.identifier.0)
        }
        Terminal::Goto { block, variant, .. } => {
            format!("goto bb{} ({:?})", block.0, variant)
        }
        Terminal::If { test, consequent, alternate, fallthrough, .. } => {
            format!(
                "if $t{} → bb{} else bb{} (fallthrough: bb{})",
                test.identifier.0, consequent.0, alternate.0, fallthrough.0
            )
        }
        Terminal::Throw { value, .. } => {
            format!("throw $t{}", value.identifier.0)
        }
        Terminal::Unsupported { .. } => "unsupported".into(),
        Terminal::Unreachable { .. } => "unreachable".into(),
        Terminal::MaybeThrow { continuation, handler, .. } => {
            format!(
                "maybe-throw → bb{}, handler: {:?}",
                continuation.0,
                handler.map(|h| h.0)
            )
        }
        Terminal::While { test, loop_, fallthrough, .. } => {
            format!("while test:bb{} loop:bb{} fallthrough:bb{}", test.0, loop_.0, fallthrough.0)
        }
        Terminal::For { init, test, loop_, fallthrough, .. } => {
            format!("for init:bb{} test:bb{} loop:bb{} fallthrough:bb{}", init.0, test.0, loop_.0, fallthrough.0)
        }
        _ => format!("{:?}", std::mem::discriminant(terminal)),
    }
}
