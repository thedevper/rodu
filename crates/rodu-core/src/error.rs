use std::fmt;

/// What kind of failure a [`RoduError`] is. Callers map it to an exit code, an HTTP status or an
/// MCP tool error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    NotFound,
    Invalid,
    Conflict,
    RuleViolation,
    Limit,
    /// An unexpected failure (storage, I/O). Its message is for logs, never for remote callers.
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::NotFound => "not_found",
            ErrorCode::Invalid => "invalid",
            ErrorCode::Conflict => "conflict",
            ErrorCode::RuleViolation => "rule_violation",
            ErrorCode::Limit => "limit",
            ErrorCode::Internal => "internal",
        }
    }
}

/// A domain error that the CLI and MCP can show as-is. `hint` tells a person or an agent how to
/// fix the request, so an agent can retry without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoduError {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
}

impl RoduError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), hint: None }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }

    pub fn rule_violation(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::RuleViolation, message)
    }

    pub fn limit(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Limit, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

impl fmt::Display for RoduError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RoduError {}

pub type Result<T, E = RoduError> = std::result::Result<T, E>;
