//! What can go wrong between the loop and a model, split by whether asking
//! again is a sensible thing to do.
//!
//! The distinction matters more than the detail: a rate limit and a model that
//! does not exist look similar in a log and want opposite responses. A provider
//! that reports the wrong kind makes an agent either hammer a server that is
//! telling it to slow down, or give up on a request that would have worked.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    /// The provider could not be reached or could not be started.
    #[error("cannot reach {target}: {message}")]
    Unreachable { target: String, message: String },

    /// The request was understood and refused: no key, wrong key, no permission.
    #[error("the provider refused to answer ({status})")]
    Unauthorized { status: String },

    #[error("the provider is busy: {message}")]
    RateLimited {
        message: String,
        /// Seconds the server asked us to wait, if it said.
        retry_after_seconds: Option<u64>,
    },

    /// The reply is not something this crate can read: not JSON, missing a field
    /// we require, an empty candidate list.
    #[error("the provider's reply is not usable: {message}")]
    Malformed { message: String },

    /// The prompt plus the reply budget exceeds what the model can hold. This is
    /// not retryable at this size: the caller has to cut context.
    #[error("the prompt needs {wanted} tokens but this model holds {available}")]
    ContextTooLong { wanted: usize, available: usize },

    /// The configuration section itself is what is wrong: no path, no URL, a key
    /// variable that is not set. A retry cannot edit a config file, so this is
    /// the one family of provider errors that is always a message for the human.
    #[error("not configured: {message}")]
    Unconfigured { message: String },

    /// This build cannot make this kind of provider, because the feature that
    /// compiles it was not asked for.
    #[error("{kind} providers need this build to be compiled with --features {feature}")]
    NotCompiled { kind: String, feature: String },

    /// This repository does not implement this kind of provider yet.
    #[error("{} providers are not implemented yet{}", .0, .1.as_deref().map(|n| format!(": {n}")).unwrap_or_default())]
    NotSupported(String, Option<String>),

    /// The human stopped it.
    #[error("the run was cancelled")]
    Cancelled,

    #[error("the provider wrote an unexpected error: {message}")]
    Other { message: String },
}

impl ProviderError {
    /// Whether the loop may ask again. `false` on every variant that describes
    /// the request rather than the connection, because retrying a bad request
    /// produces the same bad answer four times.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Unreachable { .. } | Self::RateLimited { .. } | Self::Other { .. }
        )
    }

    /// Whether the answer might be different if we wait. Only a rate limit says
    /// how long.
    pub fn backoff_hint(&self) -> Option<u64> {
        match self {
            Self::RateLimited {
                retry_after_seconds,
                ..
            } => *retry_after_seconds,
            _ => None,
        }
    }

    pub fn not_compiled(kind: impl Into<String>, feature: impl Into<String>) -> Self {
        Self::NotCompiled {
            kind: kind.into(),
            feature: feature.into(),
        }
    }

    pub fn not_supported(kind: impl Into<String>, why: Option<String>) -> Self {
        Self::NotSupported(kind.into(), why)
    }

    /// Refuse a configuration section, naming the key that makes it unusable.
    pub fn unconfigured(message: impl Into<String>) -> Self {
        Self::Unconfigured {
            message: message.into(),
        }
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_connection_problem_is_worth_repeating() {
        assert!(ProviderError::Unreachable {
            target: "http://localhost:11434".to_owned(),
            message: "connection refused".to_owned(),
        }
        .retryable());
        assert!(ProviderError::RateLimited {
            message: "too many requests".to_owned(),
            retry_after_seconds: Some(3),
        }
        .retryable());

        // Each of these describes the request, so asking again changes nothing.
        assert!(!ProviderError::Unauthorized {
            status: "401".to_owned(),
        }
        .retryable());
        assert!(!ProviderError::ContextTooLong {
            wanted: 9000,
            available: 4096,
        }
        .retryable());
        assert!(!ProviderError::not_compiled("gguf", "gguf").retryable());
        assert!(!ProviderError::not_supported("candle", None).retryable());
        assert!(!ProviderError::Cancelled.retryable());
        // A config file does not fix itself between one attempt and the next.
        assert!(!ProviderError::unconfigured("x").retryable());
    }

    #[test]
    fn a_configuration_problem_keeps_the_key_it_was_upset_about() {
        let error = ProviderError::unconfigured("providers.local.path is not a .gguf file");
        assert_eq!(
            error.to_string(),
            "not configured: providers.local.path is not a .gguf file"
        );
    }

    #[test]
    fn a_missing_feature_names_the_flag_that_fixes_it() {
        let error = ProviderError::not_compiled("gguf", "llama-cpp");
        assert!(
            error.to_string().contains("--features llama-cpp"),
            "{error}"
        );
        assert!(error.to_string().starts_with("gguf providers"), "{error}");
    }

    #[test]
    fn a_rate_limit_passes_on_the_wait_it_was_asked_for() {
        let hinted = ProviderError::RateLimited {
            message: "slow down".to_owned(),
            retry_after_seconds: Some(7),
        };
        assert_eq!(hinted.backoff_hint(), Some(7));
        let silent = ProviderError::Unreachable {
            target: "x".to_owned(),
            message: "y".to_owned(),
        };
        assert_eq!(silent.backoff_hint(), None);
    }

    #[test]
    fn an_unimplemented_kind_says_so_plainly() {
        let error = ProviderError::not_supported("onnx", Some("no runtime is linked".to_owned()));
        assert_eq!(
            error.to_string(),
            "onnx providers are not implemented yet: no runtime is linked"
        );
    }
}
