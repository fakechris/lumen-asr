//! Text injection orchestration.
//!
//! Product default: **paste-first** with clipboard restore, then type, then AX.
//! Platform backends implement [`TextInjectorBackend`].

use async_trait::async_trait;
use lumen_core::InsertStrategy;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InjectError {
    #[error("all strategies failed: {0}")]
    AllFailed(String),
    #[error("not supported: {0}")]
    NotSupported(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectMode {
    /// Paste first (default product behavior), then type, then AX.
    Auto,
    Paste,
    Ax,
    Type,
    /// Do not inject; caller should leave text on clipboard / UI only.
    CopyOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertPolicy {
    pub mode: InjectMode,
    pub preserve_clipboard: bool,
    /// When mode is Auto, try paste before AX. Default true.
    pub paste_first: bool,
}

impl Default for InsertPolicy {
    fn default() -> Self {
        Self {
            mode: InjectMode::Auto,
            preserve_clipboard: true,
            // Prefer clipboard paste; field retained for explicit type-first compatibility.
            paste_first: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertOutcome {
    pub strategy: InsertStrategy,
    pub restored_clipboard: bool,
}

/// Platform-specific insert primitives.
#[async_trait]
pub trait TextInjectorBackend: Send + Sync {
    async fn paste_with_restore(&self, text: &str, preserve: bool) -> Result<(), InjectError>;
    async fn ax_insert(&self, text: &str) -> Result<(), InjectError>;
    async fn type_unicode(&self, text: &str) -> Result<(), InjectError>;
    async fn copy_only(&self, text: &str) -> Result<(), InjectError>;
}

/// High-level injector used by the session orchestrator.
pub struct TextInjector<B: TextInjectorBackend> {
    backend: B,
    policy: InsertPolicy,
}

impl<B: TextInjectorBackend> TextInjector<B> {
    pub fn new(backend: B, policy: InsertPolicy) -> Self {
        Self { backend, policy }
    }

    pub fn policy(&self) -> &InsertPolicy {
        &self.policy
    }

    pub async fn insert(&self, text: &str) -> Result<InsertOutcome, InjectError> {
        if text.is_empty() {
            return Ok(InsertOutcome {
                strategy: InsertStrategy::None,
                restored_clipboard: false,
            });
        }

        match self.policy.mode {
            InjectMode::CopyOnly => {
                self.backend.copy_only(text).await?;
                Ok(InsertOutcome {
                    strategy: InsertStrategy::CopyOnly,
                    restored_clipboard: false,
                })
            }
            InjectMode::Paste => self.try_paste(text).await,
            InjectMode::Ax => self.try_ax(text).await,
            InjectMode::Type => self.try_type(text).await,
            InjectMode::Auto => self.try_auto(text).await,
        }
    }

    async fn try_auto(&self, text: &str) -> Result<InsertOutcome, InjectError> {
        // paste_first=true (default): clipboard ⌘V first (most reliable for terminals).
        // paste_first=false: type unicode first, then paste.
        let sequence: Vec<InsertStrategy> = if self.policy.paste_first {
            vec![
                InsertStrategy::Paste,
                InsertStrategy::Type,
                InsertStrategy::Ax,
            ]
        } else {
            vec![
                InsertStrategy::Type,
                InsertStrategy::Paste,
                InsertStrategy::Ax,
            ]
        };

        let mut errors = Vec::new();
        for strategy in sequence {
            let result = match strategy {
                InsertStrategy::Paste => self.try_paste(text).await,
                InsertStrategy::Ax => self.try_ax(text).await,
                InsertStrategy::Type => self.try_type(text).await,
                _ => continue,
            };
            match result {
                Ok(o) => return Ok(o),
                Err(e) => {
                    tracing::warn!(?strategy, error = %e, "inject strategy failed");
                    errors.push(format!("{strategy:?}: {e}"));
                }
            }
        }

        Err(InjectError::AllFailed(errors.join("; ")))
    }

    async fn try_paste(&self, text: &str) -> Result<InsertOutcome, InjectError> {
        self.backend
            .paste_with_restore(text, self.policy.preserve_clipboard)
            .await?;
        Ok(InsertOutcome {
            strategy: InsertStrategy::Paste,
            restored_clipboard: self.policy.preserve_clipboard,
        })
    }

    async fn try_ax(&self, text: &str) -> Result<InsertOutcome, InjectError> {
        self.backend.ax_insert(text).await?;
        Ok(InsertOutcome {
            strategy: InsertStrategy::Ax,
            restored_clipboard: false,
        })
    }

    async fn try_type(&self, text: &str) -> Result<InsertOutcome, InjectError> {
        self.backend.type_unicode(text).await?;
        Ok(InsertOutcome {
            strategy: InsertStrategy::Type,
            restored_clipboard: false,
        })
    }
}

/// Stub backend for unit tests / non-mac CI.
#[derive(Default)]
pub struct StubInjectorBackend {
    pub fail_paste: bool,
    pub fail_ax: bool,
    pub fail_type: bool,
}

#[async_trait]
impl TextInjectorBackend for StubInjectorBackend {
    async fn paste_with_restore(&self, _text: &str, _preserve: bool) -> Result<(), InjectError> {
        if self.fail_paste {
            Err(InjectError::Other("paste failed".into()))
        } else {
            Ok(())
        }
    }

    async fn ax_insert(&self, _text: &str) -> Result<(), InjectError> {
        if self.fail_ax {
            Err(InjectError::Other("ax failed".into()))
        } else {
            Ok(())
        }
    }

    async fn type_unicode(&self, _text: &str) -> Result<(), InjectError> {
        if self.fail_type {
            Err(InjectError::Other("type failed".into()))
        } else {
            Ok(())
        }
    }

    async fn copy_only(&self, _text: &str) -> Result<(), InjectError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_paste_first_succeeds_by_default() {
        let inj = TextInjector::new(StubInjectorBackend::default(), InsertPolicy::default());
        let o = inj.insert("hi").await.unwrap();
        assert_eq!(o.strategy, InsertStrategy::Paste);
        assert!(o.restored_clipboard);
    }

    #[tokio::test]
    async fn auto_type_first_succeeds_when_configured() {
        let inj = TextInjector::new(
            StubInjectorBackend::default(),
            InsertPolicy {
                mode: InjectMode::Auto,
                preserve_clipboard: true,
                paste_first: false,
            },
        );
        let o = inj.insert("hi").await.unwrap();
        assert_eq!(o.strategy, InsertStrategy::Type);
        assert!(!o.restored_clipboard);
    }

    #[tokio::test]
    async fn auto_paste_first_falls_back_to_type_then_ax() {
        let inj = TextInjector::new(
            StubInjectorBackend {
                fail_paste: true,
                fail_type: false,
                fail_ax: true,
            },
            InsertPolicy::default(),
        );
        let o = inj.insert("hi").await.unwrap();
        assert_eq!(o.strategy, InsertStrategy::Type);

        let inj = TextInjector::new(
            StubInjectorBackend {
                fail_paste: true,
                fail_type: true,
                fail_ax: false,
            },
            InsertPolicy::default(),
        );
        let o = inj.insert("hi").await.unwrap();
        assert_eq!(o.strategy, InsertStrategy::Ax);
    }

    #[tokio::test]
    async fn auto_type_first_falls_back_to_paste() {
        let inj = TextInjector::new(
            StubInjectorBackend {
                fail_paste: false,
                fail_type: true,
                fail_ax: true,
            },
            InsertPolicy {
                mode: InjectMode::Auto,
                preserve_clipboard: true,
                paste_first: false,
            },
        );
        let o = inj.insert("hi").await.unwrap();
        assert_eq!(o.strategy, InsertStrategy::Paste);
    }

    #[tokio::test]
    async fn auto_reports_all_failures_in_attempt_order() {
        let inj = TextInjector::new(
            StubInjectorBackend {
                fail_paste: true,
                fail_type: true,
                fail_ax: true,
            },
            InsertPolicy::default(),
        );
        let error = inj.insert("hi").await.unwrap_err().to_string();
        assert_eq!(
            error,
            "all strategies failed: Paste: paste failed; Type: type failed; Ax: ax failed"
        );
    }

    #[tokio::test]
    async fn empty_text_is_a_noop() {
        let inj = TextInjector::new(
            StubInjectorBackend {
                fail_paste: true,
                fail_type: true,
                fail_ax: true,
            },
            InsertPolicy::default(),
        );
        let outcome = inj.insert("").await.unwrap();
        assert_eq!(outcome.strategy, InsertStrategy::None);
        assert!(!outcome.restored_clipboard);
    }
}
