use crate::hir::hir::{Place, ValueReason};

/// Describes how a call or instruction affects the aliasing/mutability of values.
/// This is the core data structure used by the aliasing inference pass.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AliasingEffect {
    /// The value is frozen (made immutable)
    Freeze { value: Place, reason: ValueReason },

    /// The value is mutated
    Mutate { value: Place },

    /// The value and everything transitively captured by it are mutated
    MutateTransitive { value: Place },

    /// The value may be mutated
    MutateConditionally { value: Place },

    /// The value and everything transitively captured by it may be mutated
    MutateTransitiveConditionally { value: Place },

    /// `from` is captured into `to` (mutable capture)
    Capture { from: Place, into: Place },

    /// `from` is immutably captured into `to`
    ImmutableCapture { from: Place, into: Place },

    /// `into` aliases `from`
    Alias { from: Place, into: Place },

    /// `into` may alias `from`
    MaybeAlias { from: Place, into: Place },

    /// `into` is assigned from `from`
    Assign { from: Place, into: Place },

    /// `into` is created from `from`
    CreateFrom { from: Place, into: Place },

    /// A new value is created at `into`
    Create { into: Place },

    /// A function is created with the listed captures
    CreateFunction {
        into: Place,
        captures: Vec<Place>,
    },

    /// A function application: receiver calls function with args, result goes into `into`
    Apply {
        receiver: Place,
        function: Place,
        args: Vec<Place>,
        into: Place,
    },

    /// The place is used in a render context (JSX props, return value)
    Render { value: Place },

    /// The place is impure (e.g., result of Date.now(), ref.current)
    Impure { value: Place },
}

impl AliasingEffect {
    /// Returns all Place references involved in this effect.
    pub fn places(&self) -> Vec<&Place> {
        match self {
            AliasingEffect::Freeze { value, .. }
            | AliasingEffect::Mutate { value }
            | AliasingEffect::MutateTransitive { value }
            | AliasingEffect::MutateConditionally { value }
            | AliasingEffect::MutateTransitiveConditionally { value }
            | AliasingEffect::Render { value }
            | AliasingEffect::Impure { value } => vec![value],

            AliasingEffect::Capture { from, into }
            | AliasingEffect::ImmutableCapture { from, into }
            | AliasingEffect::Alias { from, into }
            | AliasingEffect::MaybeAlias { from, into }
            | AliasingEffect::Assign { from, into }
            | AliasingEffect::CreateFrom { from, into } => vec![from, into],

            AliasingEffect::Create { into } => vec![into],

            AliasingEffect::CreateFunction { into, captures } => {
                let mut places = vec![into];
                places.extend(captures.iter());
                places
            }

            AliasingEffect::Apply { receiver, function, args, into } => {
                let mut places = vec![receiver, function, into];
                places.extend(args.iter());
                places
            }
        }
    }
}
