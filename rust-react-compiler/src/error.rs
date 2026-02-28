use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Unsupported but known pattern — graceful bailout
    Todo,
    /// Unexpected/invalid state — hard invariant violation
    Invariant,
    /// Invalid user code that the compiler detected
    InvalidJS,
    InvalidReact,
    InvalidConfig,
    Syntax,
}

#[derive(Debug, Clone)]
pub struct CompilerDiagnostic {
    pub category: ErrorCategory,
    pub message: String,
    /// Source span (byte offsets)
    pub span: Option<(u32, u32)>,
    pub suggestions: Vec<String>,
}

impl CompilerDiagnostic {
    pub fn new(category: ErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            span: None,
            suggestions: Vec::new(),
        }
    }

    pub fn with_span(mut self, start: u32, end: u32) -> Self {
        self.span = Some((start, end));
        self
    }
}

#[derive(Debug, Error, Clone)]
#[error("{}", format_errors(&self.0))]
pub struct CompilerError(pub Vec<CompilerDiagnostic>);

fn format_errors(diags: &[CompilerDiagnostic]) -> String {
    diags
        .iter()
        .map(|d| format!("[{:?}] {}", d.category, d.message))
        .collect::<Vec<_>>()
        .join("\n")
}

impl CompilerError {
    pub fn todo(message: impl Into<String>) -> Self {
        Self(vec![CompilerDiagnostic::new(ErrorCategory::Todo, message)])
    }

    pub fn invariant(message: impl Into<String>) -> Self {
        Self(vec![CompilerDiagnostic::new(ErrorCategory::Invariant, message)])
    }

    pub fn invalid_js(message: impl Into<String>) -> Self {
        Self(vec![CompilerDiagnostic::new(ErrorCategory::InvalidJS, message)])
    }

    pub fn invalid_react(message: impl Into<String>) -> Self {
        Self(vec![CompilerDiagnostic::new(ErrorCategory::InvalidReact, message)])
    }

    pub fn from_diagnostics(diags: Vec<CompilerDiagnostic>) -> Self {
        Self(diags)
    }
}

pub type Result<T> = std::result::Result<T, CompilerError>;
