use std::collections::{HashMap, HashSet};
use crate::hir::hir::{
    BlockId, DeclarationId, Identifier, IdentifierId, InstructionId,
    ReactFunctionType, ReactiveScope, ScopeId, SourceLocation,
};

use crate::error::{CompilerDiagnostic, CompilerError};

// ---------------------------------------------------------------------------
// Compiler options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EnvironmentConfig {
    /// Enable memoization (reactive scopes → useMemo/useCallback)
    pub enable_memoization: bool,
    /// Enable validation passes
    pub enable_validations: bool,
    /// Output mode
    pub output_mode: OutputMode,
    /// Drop manual memoization (useMemo/useCallback)
    pub enable_drop_manual_memoization: bool,
    /// Enable function outlining
    pub enable_function_outlining: bool,
    /// Enable JSX outlining
    pub enable_jsx_outlining: bool,
    /// Enable naming anonymous functions
    pub enable_name_anonymous_functions: bool,
    /// Validate hooks usage
    pub validate_hooks_usage: bool,
    /// Validate no capitalized calls
    pub validate_no_capitalized_calls: bool,
    /// Validate ref access during render
    pub validate_ref_access_during_render: bool,
    /// Validate no setState in render
    pub validate_no_set_state_in_render: bool,
    /// Validate exhaustive memo deps
    pub validate_exhaustive_memoization_dependencies: bool,
    /// Assert valid mutable ranges
    pub assert_valid_mutable_ranges: bool,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        EnvironmentConfig {
            enable_memoization: true,
            enable_validations: true,
            output_mode: OutputMode::Function,
            enable_drop_manual_memoization: true,
            enable_function_outlining: true,
            enable_jsx_outlining: false,
            enable_name_anonymous_functions: false,
            validate_hooks_usage: true,
            validate_no_capitalized_calls: true,
            validate_ref_access_during_render: false,
            validate_no_set_state_in_render: false,
            validate_exhaustive_memoization_dependencies: false,
            assert_valid_mutable_ranges: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Emit the compiled function
    Function,
    /// Lint-only mode (don't transform)
    Lint,
    /// Server-side rendering optimizations
    Ssr,
}

// ---------------------------------------------------------------------------
// Environment — tracks compilation state, ID counters, errors
// ---------------------------------------------------------------------------

pub struct Environment {
    pub config: EnvironmentConfig,
    pub fn_type: ReactFunctionType,
    pub filename: Option<String>,

    // ID counters
    next_identifier_id: u32,
    next_block_id: u32,
    next_instruction_id: u32,
    next_scope_id: u32,
    next_declaration_id: u32,

    // Arena of all identifiers by IdentifierId
    pub identifiers: HashMap<IdentifierId, Identifier>,

    // Arena of all reactive scopes by ScopeId
    pub scopes: HashMap<ScopeId, ReactiveScope>,

    // Accumulated non-fatal diagnostics
    errors: Vec<CompilerDiagnostic>,

    // Outlined functions collected by the outline_functions pass.
    // Each entry is (name, declaration_text) e.g. ("_temp", "function _temp(x_0) { return x_0; }")
    pub outlined_functions: Vec<(String, String)>,

    // Module-level variable names collected during lowering.
    // These are let/const/var declarations at module scope (not inside any function).
    // Used by outline_functions to determine if free variables are safe to capture
    // from an outlined (hoisted) function.
    pub module_level_names: HashSet<String>,
}

impl Environment {
    pub fn new(
        fn_type: ReactFunctionType,
        config: EnvironmentConfig,
        filename: Option<String>,
    ) -> Self {
        Environment {
            config,
            fn_type,
            filename,
            next_identifier_id: 0,
            next_block_id: 0,
            next_instruction_id: 0,
            next_scope_id: 0,
            next_declaration_id: 0,
            identifiers: HashMap::new(),
            scopes: HashMap::new(),
            errors: Vec::new(),
            outlined_functions: Vec::new(),
            module_level_names: HashSet::new(),
        }
    }

    // --- ID factories ---

    pub fn new_identifier_id(&mut self) -> IdentifierId {
        let id = IdentifierId(self.next_identifier_id);
        self.next_identifier_id += 1;
        id
    }

    pub fn new_declaration_id(&mut self) -> DeclarationId {
        let id = DeclarationId(self.next_declaration_id);
        self.next_declaration_id += 1;
        id
    }

    pub fn new_block_id(&mut self) -> BlockId {
        let id = BlockId(self.next_block_id);
        self.next_block_id += 1;
        id
    }

    pub fn new_instruction_id(&mut self) -> InstructionId {
        let id = InstructionId(self.next_instruction_id);
        self.next_instruction_id += 1;
        id
    }

    pub fn new_scope_id(&mut self) -> ScopeId {
        let id = ScopeId(self.next_scope_id);
        self.next_scope_id += 1;
        id
    }

    /// Allocate a new temporary identifier and register it.
    pub fn new_temporary(&mut self, loc: SourceLocation) -> IdentifierId {
        let id = self.new_identifier_id();
        let ident = Identifier::new_temporary(id, loc);
        self.identifiers.insert(id, ident);
        id
    }

    pub fn get_identifier(&self, id: IdentifierId) -> Option<&Identifier> {
        self.identifiers.get(&id)
    }

    pub fn get_identifier_mut(&mut self, id: IdentifierId) -> Option<&mut Identifier> {
        self.identifiers.get_mut(&id)
    }

    // --- Error accumulation ---

    pub fn record_error(&mut self, diag: CompilerDiagnostic) {
        self.errors.push(diag);
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn aggregate_errors(&self) -> CompilerError {
        CompilerError::from_diagnostics(self.errors.clone())
    }

    /// Run a validation pass; catch non-invariant CompilerErrors and record them.
    pub fn try_record<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Environment) -> Result<(), CompilerError>,
    {
        if let Err(e) = f(self) {
            for diag in e.0 {
                // Only record Todo/InvalidJS/InvalidReact errors (not Invariant)
                self.errors.push(diag);
            }
        }
    }

    // --- Feature flags (delegate to config) ---

    pub fn enable_memoization(&self) -> bool { self.config.enable_memoization }
    pub fn enable_validations(&self) -> bool { self.config.enable_validations }
    pub fn output_mode(&self) -> OutputMode { self.config.output_mode }
}
