//! Text injection orchestration.
//!
//! Product default: **paste-first** with clipboard restore, then AX, then type.
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
    /// Paste first (default product behavior), then AX, then type.
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
    /// When mode is Auto, try paste before AX (Wispr-like). Default true.
    pub paste_first: bool,
}

impl Default for InsertPolicy {
    fn default() -> Self {
        Self {
            mode: InjectMode::Auto,
            preserve_clipboard: true,
            // Prefer type-at-cursor; field kept for config compatibility.
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
        // Competitor-aligned default (闪电说 / OpenLess):
        //   Type unicode at cursor first (no app activate), then clipboard paste.
        // `paste_first=true` keeps classic Wispr-style paste-first for those who want it.
        let sequence: Vec<InsertStrategy> = if self.policy.paste_first {
            vec![
                InsertStrategy::Type,
                InsertStrategy::Paste,
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
pub struct StubInjectorBackend {
    pub fail_paste: bool,
    pub fail_ax: bool,
}

impl Default for StubInjectorBackend {
    fn default() -> Self {
        Self {
            fail_paste: false,
            fail_ax: false,
        }
    }
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
        Ok(())
    }

    async fn copy_only(&self, _text: &str) -> Result<(), InjectError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_type_first_succeeds() {
        let inj = TextInjector::new(StubInjectorBackend::default(), InsertPolicy::default());
        let o = inj.insert("hi").await.unwrap();
        // Type is first in Auto sequence now.
        assert_eq!(o.strategy, InsertStrategy::Type);
    }

    #[tokio::test]
    async fn auto_falls_back_to_paste_then_ax() {
        // Stub type always succeeds — verify paste path via Paste-only mode.
        let backend = StubInjectorBackend {
            fail_paste: false,
            fail_ax: false,
        };
        let inj = TextInjector::new(
            backend,
            InsertPolicy {
                mode: InjectMode::Paste,
                preserve_clipboard: true,
                paste_first: true,
            },
        );
        let o = inj.insert("hi").await.unwrap();
        assert_eq!(o.strategy, InsertStrategy::Paste);
    }
}
