use std::fmt;

use crate::SensitiveText;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationCertainty {
    NotSubmitted,
    Indeterminate,
}

pub enum MutationError {
    InvalidRequest(SensitiveText),
    RequestRejected(SensitiveText),
    Configuration(SensitiveText),
    RateLimited(SensitiveText),
    Unavailable(SensitiveText),
    RateLimitedIndeterminate(SensitiveText),
    Indeterminate(SensitiveText),
}

impl MutationError {
    pub fn certainty(&self) -> MutationCertainty {
        match self {
            Self::RateLimitedIndeterminate(_) | Self::Indeterminate(_) => {
                MutationCertainty::Indeterminate
            }
            Self::InvalidRequest(_)
            | Self::RequestRejected(_)
            | Self::Configuration(_)
            | Self::RateLimited(_)
            | Self::Unavailable(_) => MutationCertainty::NotSubmitted,
        }
    }

    pub fn detail(&self) -> &SensitiveText {
        match self {
            Self::InvalidRequest(detail)
            | Self::RequestRejected(detail)
            | Self::Configuration(detail)
            | Self::RateLimited(detail)
            | Self::Unavailable(detail)
            | Self::RateLimitedIndeterminate(detail)
            | Self::Indeterminate(detail) => detail,
        }
    }
}

impl fmt::Debug for MutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest(_) => "MutationError::InvalidRequest([redacted])",
            Self::RequestRejected(_) => "MutationError::RequestRejected([redacted])",
            Self::Configuration(_) => "MutationError::Configuration([redacted])",
            Self::RateLimited(_) => "MutationError::RateLimited([redacted])",
            Self::Unavailable(_) => "MutationError::Unavailable([redacted])",
            Self::RateLimitedIndeterminate(_) => {
                "MutationError::RateLimitedIndeterminate([redacted])"
            }
            Self::Indeterminate(_) => "MutationError::Indeterminate([redacted])",
        })
    }
}

impl fmt::Display for MutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest(_) => "NMI mutation request is invalid",
            Self::RequestRejected(_) => "NMI rejected the mutation before processing",
            Self::Configuration(_) => "NMI mutation configuration is invalid",
            Self::RateLimited(_) => "NMI mutation request was rate limited before processing",
            Self::Unavailable(_) => "NMI mutation service is unavailable",
            Self::RateLimitedIndeterminate(_) => {
                "NMI mutation was rate limited with an indeterminate outcome"
            }
            Self::Indeterminate(_) => "NMI mutation outcome is indeterminate",
        })
    }
}

impl std::error::Error for MutationError {}

pub enum QueryError {
    InvalidRequest(SensitiveText),
    MalformedResponse(SensitiveText),
    Configuration(SensitiveText),
    RateLimited(SensitiveText),
    Unavailable(SensitiveText),
}

impl QueryError {
    pub fn detail(&self) -> &SensitiveText {
        match self {
            Self::InvalidRequest(detail)
            | Self::MalformedResponse(detail)
            | Self::Configuration(detail)
            | Self::RateLimited(detail)
            | Self::Unavailable(detail) => detail,
        }
    }
}

impl fmt::Debug for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest(_) => "QueryError::InvalidRequest([redacted])",
            Self::MalformedResponse(_) => "QueryError::MalformedResponse([redacted])",
            Self::Configuration(_) => "QueryError::Configuration([redacted])",
            Self::RateLimited(_) => "QueryError::RateLimited([redacted])",
            Self::Unavailable(_) => "QueryError::Unavailable([redacted])",
        })
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest(_) => "NMI query request is invalid",
            Self::MalformedResponse(_) => "NMI query response is malformed",
            Self::Configuration(_) => "NMI query configuration is invalid",
            Self::RateLimited(_) => "NMI query request was rate limited",
            Self::Unavailable(_) => "NMI query service is unavailable",
        })
    }
}

impl std::error::Error for QueryError {}
