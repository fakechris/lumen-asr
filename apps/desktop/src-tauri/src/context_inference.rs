use lumen_context::{AxNode, ContextManifest};
use std::collections::HashSet;

const APP_LIMIT: usize = 100;
const TITLE_LIMIT: usize = 180;
const EDITOR_META_LIMIT: usize = 120;
const SELECTED_LIMIT: usize = 400;
const CURSOR_PREFIX_LIMIT: usize = 600;
const CURSOR_SUFFIX_LIMIT: usize = 400;
const NEARBY_LIMIT: usize = 300;
const VISIBLE_BLOCK_LIMIT: usize = 400;
pub(crate) const CORRECTOR_CONTEXT_MAX_CHARS: usize = 2_000;

/// Build a compact, text-only view for the corrector.
///
/// The projection intentionally excludes URLs, coordinates, trees, source refs,
/// screenshots, OCR metadata and capture diagnostics. Those remain in the local
/// manifest and are not useful to a text correction model.
pub(crate) fn flatten_for_corrector(
    manifest: &ContextManifest,
    max_chars: usize,
) -> Option<String> {
    if max_chars == 0 || !manifest.privacy.raw_text_allowed {
        return None;
    }

    let mut out = FlatTextBuilder::new(max_chars);

    let editor = manifest.editor.as_ref();
    let browser_element = manifest
        .browser
        .as_ref()
        .and_then(|browser| browser.focused_element.as_ref());
    let secure_editor = editor.is_some_and(|value| value.secure)
        || browser_element.is_some_and(|value| value.secure);
    if secure_editor {
        return None;
    }

    let browser = manifest.browser.as_ref();
    let selected = editor
        .and_then(|value| non_empty(value.selected_text.as_deref()))
        .or_else(|| browser.and_then(|value| non_empty(value.selection_text.as_deref())));
    let prefix = editor
        .and_then(|value| non_empty(value.cursor_prefix.as_deref()))
        .or_else(|| browser.and_then(|value| non_empty(value.nearby_before.as_deref())));
    let suffix = editor
        .and_then(|value| non_empty(value.cursor_suffix.as_deref()))
        .or_else(|| browser.and_then(|value| non_empty(value.nearby_after.as_deref())));

    out.push_first("选中文字", selected, SELECTED_LIMIT);
    out.push_first("光标后", suffix, CURSOR_SUFFIX_LIMIT);
    out.push_last("光标前", prefix, CURSOR_PREFIX_LIMIT);

    if prefix.is_none() && suffix.is_none() {
        let full_field = editor
            .and_then(|value| non_empty(value.full_field_text.as_deref()))
            .or_else(|| browser_element.and_then(|value| non_empty(value.value.as_deref())));
        out.push_first(
            "输入框全文",
            full_field,
            CURSOR_PREFIX_LIMIT + CURSOR_SUFFIX_LIMIT,
        );
    }
    out.remember(
        editor
            .and_then(|value| value.full_field_text.as_deref())
            .or_else(|| browser_element.and_then(|value| value.value.as_deref())),
    );

    if let Some(target) = manifest.target.as_ref() {
        out.push_first("应用", target.app_name.as_deref(), APP_LIMIT);
        out.push_first("窗口", target.window_title.as_deref(), TITLE_LIMIT);
    }

    if let Some(browser) = manifest.browser.as_ref() {
        out.push_first("网页", browser.title.as_deref(), TITLE_LIMIT);
        // Deliberately use the parsed domain only. Full URLs may contain paths,
        // query values, fragments, document ids or access tokens.
        out.push_first("域名", browser.domain.as_deref(), APP_LIMIT);
    }

    if let Some(editor) = editor {
        out.push_first("输入框角色", editor.role.as_deref(), EDITOR_META_LIMIT);
        out.push_first("输入框名称", editor.label.as_deref(), EDITOR_META_LIMIT);
        out.push_first(
            "输入框提示",
            editor.placeholder.as_deref(),
            EDITOR_META_LIMIT,
        );
    } else if let Some(element) = browser_element {
        out.push_first("输入框角色", element.role.as_deref(), EDITOR_META_LIMIT);
        out.push_first(
            "输入框名称",
            element.aria_label.as_deref(),
            EDITOR_META_LIMIT,
        );
        out.push_first(
            "输入框提示",
            element.placeholder.as_deref(),
            EDITOR_META_LIMIT,
        );
    }

    let nearby_before = editor
        .and_then(|value| non_empty(value.nearby_before.as_deref()))
        .or_else(|| browser_element.and_then(|value| non_empty(value.sibling_before.as_deref())));
    let nearby_after = editor
        .and_then(|value| non_empty(value.nearby_after.as_deref()))
        .or_else(|| browser_element.and_then(|value| non_empty(value.sibling_after.as_deref())));
    out.push_last("输入框前文", nearby_before, NEARBY_LIMIT);
    out.push_first("输入框后文", nearby_after, NEARBY_LIMIT);

    match manifest.visible_text_fused.as_ref() {
        Some(fused) if !fused.blocks.is_empty() => {
            for block in &fused.blocks {
                out.push_first("可见文字", Some(&block.text), VISIBLE_BLOCK_LIMIT);
            }
        }
        _ => {
            if let Some(browser) = manifest.browser.as_ref() {
                for block in &browser.viewport_text_blocks {
                    out.push_first("可见文字", Some(&block.text), VISIBLE_BLOCK_LIMIT);
                }
            }
            if let Some(ax_visible) = manifest.ax_visible.as_ref() {
                push_ax_text(&mut out, &ax_visible.roots);
            }
            for document in &manifest.ocr_documents {
                out.push_first("可见文字", Some(&document.text), VISIBLE_BLOCK_LIMIT);
            }
        }
    }

    out.finish()
}

fn push_ax_text(out: &mut FlatTextBuilder, nodes: &[AxNode]) {
    for node in nodes {
        out.push_first("可见文字", node.title.as_deref(), VISIBLE_BLOCK_LIMIT);
        out.push_first("可见文字", node.value.as_deref(), VISIBLE_BLOCK_LIMIT);
        out.push_first("可见文字", node.description.as_deref(), VISIBLE_BLOCK_LIMIT);
        push_ax_text(out, &node.children);
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

struct FlatTextBuilder {
    max_chars: usize,
    used_chars: usize,
    lines: Vec<String>,
    seen_values: HashSet<String>,
}

impl FlatTextBuilder {
    fn new(max_chars: usize) -> Self {
        Self {
            max_chars,
            used_chars: 0,
            lines: Vec::new(),
            seen_values: HashSet::new(),
        }
    }

    fn push_first(&mut self, label: &str, value: Option<&str>, field_limit: usize) {
        self.push(label, value, field_limit, false);
    }

    fn push_last(&mut self, label: &str, value: Option<&str>, field_limit: usize) {
        self.push(label, value, field_limit, true);
    }

    fn remember(&mut self, value: Option<&str>) {
        let Some(value) = value else {
            return;
        };
        let normalized = normalize_whitespace(value);
        if !normalized.is_empty() {
            self.seen_values.insert(normalized.to_lowercase());
        }
    }

    fn push(&mut self, label: &str, value: Option<&str>, field_limit: usize, keep_end: bool) {
        let Some(value) = value else {
            return;
        };
        let normalized = normalize_whitespace(value);
        if normalized.is_empty() {
            return;
        }
        let dedupe_key = normalized.to_lowercase();
        if self.seen_values.contains(&dedupe_key) {
            return;
        }

        let separator_chars = usize::from(!self.lines.is_empty());
        let prefix = format!("{label}：");
        let fixed_chars = separator_chars + prefix.chars().count();
        let Some(available) = self
            .max_chars
            .checked_sub(self.used_chars.saturating_add(fixed_chars))
        else {
            return;
        };
        if available == 0 {
            return;
        }
        let value_limit = field_limit.min(available);
        let value = if keep_end {
            take_last_chars(&normalized, value_limit)
        } else {
            take_first_chars(&normalized, value_limit)
        };
        if value.is_empty() {
            return;
        }

        self.used_chars += fixed_chars + value.chars().count();
        self.lines.push(format!("{prefix}{value}"));
        self.seen_values.insert(dedupe_key);
    }

    fn finish(self) -> Option<String> {
        (!self.lines.is_empty()).then(|| self.lines.join("\n"))
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn take_first_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn take_last_chars(value: &str, max_chars: usize) -> String {
    let length = value.chars().count();
    value
        .chars()
        .skip(length.saturating_sub(max_chars))
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use lumen_context::{
        AxNode, AxVisibleContext, BrowserContext, BrowserElementContext, CaptureDiagnostics,
        CaptureId, CaptureProfile, CaptureTrigger, ContextManifest, EditorContext, PrivacyContext,
        TargetContext, TriggerKind, VisibleTextBlock, VisibleTextDocument,
    };
    use std::collections::BTreeMap;
    use uuid::Uuid;

    use super::flatten_for_corrector;

    fn fixture_manifest() -> ContextManifest {
        let now = Utc::now();
        ContextManifest {
            schema_version: 1,
            capture_id: CaptureId::new(),
            consumer_session_id: Uuid::new_v4(),
            revision: 7,
            profile: CaptureProfile::FullLocal,
            trigger: CaptureTrigger {
                kind: TriggerKind::DictationHotkey,
                pressed_at: now,
                released_at: None,
            },
            requested_at: now,
            frozen_at: now,
            target_generation: 3,
            target: Some(TargetContext {
                app_name: Some("Chromium".into()),
                window_title: Some("Context Capture 验收 - Docs".into()),
                document_url: Some("https://docs.example.test/editor?draft=private-fixture".into()),
                ..TargetContext::default()
            }),
            system: None,
            editor: Some(EditorContext {
                role: Some("AXTextArea".into()),
                label: Some("Release note".into()),
                placeholder: Some("写发布说明".into()),
                selected_text: Some("Safari".into()),
                cursor_prefix: Some("项目进度：截图和浏览器上下文已经接入。下一步需要验证 ".into()),
                cursor_suffix: Some("。".into()),
                full_field_text: Some("FULL-FIELD-SHOULD-NOT-BE-DUPLICATED".into()),
                nearby_before: Some("验收清单：OCR engine: macOS Vision".into()),
                nearby_after: Some("发布前运行浏览器矩阵".into()),
                ..EditorContext::default()
            }),
            ax_visible: None,
            browser: Some(BrowserContext {
                title: Some("Context Capture 验收".into()),
                url: Some("https://docs.example.test/editor?draft=private-fixture".into()),
                origin: Some("https://docs.example.test".into()),
                domain: Some("docs.example.test".into()),
                focused_element: Some(BrowserElementContext {
                    value: Some("FULL-BROWSER-VALUE-SHOULD-NOT-BE-DUPLICATED".into()),
                    ..BrowserElementContext::default()
                }),
                ..BrowserContext::default()
            }),
            screenshots: vec![],
            ocr_documents: vec![],
            visible_text_fused: Some(VisibleTextDocument {
                blocks: vec![
                    VisibleTextBlock {
                        text: "验收清单：OCR engine: macOS Vision".into(),
                        source_refs: vec!["source-ref-must-not-appear".into()],
                        order: 0,
                        ..VisibleTextBlock::default()
                    },
                    VisibleTextBlock {
                        text: "Safari iframe matrix".into(),
                        order: 1,
                        ..VisibleTextBlock::default()
                    },
                    VisibleTextBlock {
                        text: "FULL-FIELD-SHOULD-NOT-BE-DUPLICATED".into(),
                        order: 2,
                        ..VisibleTextBlock::default()
                    },
                ],
                generated_at: Some(now),
                policy_version: 1,
            }),
            artifacts: vec![],
            source_status: BTreeMap::new(),
            privacy: PrivacyContext {
                raw_text_allowed: true,
                screenshots_allowed: true,
                ..PrivacyContext::default()
            },
            diagnostics: CaptureDiagnostics::default(),
        }
    }

    #[test]
    fn projects_manifest_to_bounded_flat_text_without_capture_internals() {
        let text = flatten_for_corrector(&fixture_manifest(), 2_000).unwrap();

        assert!(text.contains("应用：Chromium"));
        assert!(text.contains("窗口：Context Capture 验收 - Docs"));
        assert!(text.contains("网页：Context Capture 验收"));
        assert!(text.contains("域名：docs.example.test"));
        assert!(text.contains("选中文字：Safari"));
        assert!(text.contains("光标前：项目进度"));
        assert!(text.contains("可见文字：Safari iframe matrix"));
        assert_eq!(text.matches("OCR engine: macOS Vision").count(), 1);

        assert!(!text.contains("private-fixture"));
        assert!(!text.contains("source-ref"));
        assert!(!text.contains("FULL-FIELD"));
        assert!(!text.contains("FULL-BROWSER"));
        assert!(text.chars().count() <= 2_000);
    }

    #[test]
    fn preserves_editor_selection_before_scene_metadata_when_budget_is_tight() {
        let text = flatten_for_corrector(&fixture_manifest(), 80).unwrap();

        assert!(text.contains("选中文字：Safari"));
        assert!(text.contains("光标后：。"));
        assert!(text.chars().count() <= 80);
    }

    #[test]
    fn refuses_to_project_secure_editor_content() {
        let mut manifest = fixture_manifest();
        manifest.editor.as_mut().unwrap().secure = true;

        assert!(flatten_for_corrector(&manifest, 2_000).is_none());
    }

    #[test]
    fn flattens_ax_visible_text_when_fusion_is_not_ready() {
        let mut manifest = fixture_manifest();
        manifest.browser = None;
        manifest.visible_text_fused = None;
        manifest.ax_visible = Some(AxVisibleContext {
            roots: vec![AxNode {
                role: Some("AXStaticText".into()),
                value: Some("Native status: ready".into()),
                ..AxNode::default()
            }],
            ..AxVisibleContext::default()
        });

        let text = flatten_for_corrector(&manifest, 2_000).unwrap();
        assert!(text.contains("可见文字：Native status: ready"));
        assert!(!text.contains("AXStaticText"));
    }
}
