//! User-extensible minutes templates (SKILL.md-style).
//!
//! A minutes template is a Markdown file: a small YAML frontmatter block
//! (`name`, `description`, optional `language`) followed by a Markdown body of
//! instructions that steer the **style and emphasis** of the structured minutes
//! (e.g. "action-item oriented" vs "decision log"). The body is interpolated
//! into the minutes system prompt as an advisory section — the JSON output
//! contract and red lines in `lumen_prompts::MINUTES_SYSTEM_ZH` always win, so
//! a template can never break parsing.
//!
//! Resolution order: built-in templates are embedded in the binary; the user
//! can add or override templates by dropping `<name>.md` files into
//! `<data_dir>/minutes-templates/` (the caller supplies the directory — this
//! crate does not know the app's data-dir convention). A user template with the
//! same `name` as a built-in replaces it. Malformed template files are skipped
//! with a warning, never fatal; an unknown configured name falls back to the
//! default template.

use std::path::Path;

/// Name of the built-in default template. Selecting it (or leaving the config
/// empty) keeps the pre-template minutes behavior: its body is empty, so the
/// generated system prompt is byte-for-byte the constant prompt.
pub const DEFAULT_TEMPLATE_NAME: &str = "default";

/// A minutes template: frontmatter metadata plus the Markdown instruction body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinutesTemplate {
    /// Selection key (frontmatter `name`). Matched case-insensitively.
    pub name: String,
    /// One-line human description for the settings UI.
    pub description: String,
    /// Optional output-language hint from the frontmatter (informational only;
    /// the minutes prompt already requires matching the transcript language).
    pub language: Option<String>,
    /// Markdown instructions interpolated into the system prompt.
    pub body: String,
    /// True for templates embedded in the binary (vs. discovered on disk).
    pub builtin: bool,
}

/// Why a template file could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("missing YAML frontmatter (expected --- fences)")]
    NoFrontmatter,
    #[error("frontmatter is not closed by a --- fence")]
    UnclosedFrontmatter,
    #[error("frontmatter line is not a `key: value` pair: {0}")]
    MalformedFrontmatterLine(String),
    #[error("frontmatter `name` is missing or empty")]
    MissingName,
}

/// Parse a template from Markdown-with-frontmatter text (Kapinote SKILL.md
/// shape). The frontmatter parser is deliberately minimal — flat
/// `key: value` lines between `---` fences; unknown keys are ignored — so no
/// YAML dependency is pulled in for a three-field header.
pub fn parse_template(text: &str) -> Result<MinutesTemplate, TemplateError> {
    parse_template_with_origin(text, false)
}

fn parse_template_with_origin(text: &str, builtin: bool) -> Result<MinutesTemplate, TemplateError> {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err(TemplateError::NoFrontmatter);
    }
    let mut name = String::new();
    let mut description = String::new();
    let mut language: Option<String> = None;
    let mut body_start = None;
    for (consumed, line) in lines.by_ref().enumerate() {
        if line.trim() == "---" {
            // +2: the opening fence plus this closing fence.
            body_start = Some(consumed + 2);
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| TemplateError::MalformedFrontmatterLine(line.to_string()))?;
        let value = unquote(value.trim());
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => name = value,
            "description" => description = value,
            "language" => {
                language = (!value.is_empty()).then_some(value);
            }
            // Unknown keys are tolerated so templates stay forward-compatible.
            _ => {}
        }
    }
    let body_start = body_start.ok_or(TemplateError::UnclosedFrontmatter)?;
    if name.trim().is_empty() {
        return Err(TemplateError::MissingName);
    }
    let body = text.lines().skip(body_start).collect::<Vec<_>>().join("\n");
    Ok(MinutesTemplate {
        name: name.trim().to_string(),
        description: description.trim().to_string(),
        language,
        body: body.trim().to_string(),
        builtin,
    })
}

/// Strip one layer of surrounding quotes from a frontmatter scalar.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

// ── Built-in templates ───────────────────────────────────────────────────

/// The default template: an empty body keeps the system prompt byte-for-byte
/// the pre-template prompt (the "no template" behavior), so the existing
/// balanced minutes stay the out-of-box result.
const BUILTIN_DEFAULT: &str = r#"---
name: default
description: 默认纪要：决策、行动项、讨论要点均衡呈现
---
"#;

const BUILTIN_ACTION_ITEMS: &str = r#"---
name: action-items
description: 任务清单导向：重点提取行动项、负责人与截止时间
---
- 以 action_items 为核心：凡是会上提到的待办、跟进、分工、承诺，都尽量提取为独立的 action_item。
- 每条 action_item 尽量写明做什么；owner / due 仍然只在转录中明确出现时才填写，不得编造。
- decisions 只保留真正拍板定下来的事项；一般的讨论过程放入 discussion。
- one_liner 一句话概括这场会"接下来要做什么"。
"#;

const BUILTIN_DECISION_LOG: &str = r#"---
name: decision-log
description: 决策记录导向：完整记录每个决策及其理由与背景
---
- 以 decisions 为核心：每条决策写清"决定了什么"，会上说明过理由的一并写入 text。
- 讨论中出现过的不同方案或分歧放入 discussion，没有结论的问题放入 open_questions。
- action_items 只保留由决策直接派生的行动。
- one_liner 一句话概括本场会议最重要的决策。
"#;

const BUILTIN_WRITE_EMAIL: &str = r#"---
name: write-email
description: 会后跟进邮件：输出适合直接发给与会人的跟进内容
---
- one_liner 写成可直接放进邮件正文的 2-4 句总结，语气正式、得体。
- action_items 按负责人归并排序，措辞适合邮件里的"请你跟进"式表达。
- discussion 精简为收件人需要了解的背景要点，省略闲聊性内容。
- decisions 与 open_questions 照常提取，供邮件中引用。
"#;

const BUILTINS: &[&str] = &[
    BUILTIN_DEFAULT,
    BUILTIN_ACTION_ITEMS,
    BUILTIN_DECISION_LOG,
    BUILTIN_WRITE_EMAIL,
];

/// The templates embedded in the binary, in display order (default first).
pub fn builtin_templates() -> Vec<MinutesTemplate> {
    BUILTINS
        .iter()
        .map(|text| {
            parse_template_with_origin(text, true)
                .expect("built-in minutes templates must be valid")
        })
        .collect()
}

/// Discover user templates from a directory of `*.md` files. A missing or
/// unreadable directory simply yields no templates; a malformed file is skipped
/// with a warning — template discovery is never fatal.
pub fn load_user_templates(dir: &Path) -> Vec<MinutesTemplate> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut templates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| TemplateError::MalformedFrontmatterLine(e.to_string()))
            .and_then(|text| parse_template(&text));
        match parsed {
            Ok(template) => templates.push(template),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping malformed minutes template");
            }
        }
    }
    templates
}

/// All available templates: the built-ins, with same-named user templates
/// overriding them in place, plus any additional user templates appended.
pub fn list_templates(user_dir: Option<&Path>) -> Vec<MinutesTemplate> {
    let mut templates = builtin_templates();
    let Some(dir) = user_dir else {
        return templates;
    };
    let mut extra = Vec::new();
    for user in load_user_templates(dir) {
        if let Some(slot) = templates
            .iter_mut()
            .find(|t| t.name.eq_ignore_ascii_case(&user.name))
        {
            *slot = user;
        } else {
            extra.push(user);
        }
    }
    templates.extend(extra);
    templates
}

/// Resolve a configured template name to a template. An empty name selects the
/// default; an unknown name warns and falls back to the default, so a stale or
/// hand-edited config can never break minutes generation.
pub fn resolve_template(name: &str, user_dir: Option<&Path>) -> MinutesTemplate {
    let name = name.trim();
    if name.is_empty() {
        return builtin_templates()
            .into_iter()
            .next()
            .expect("the default built-in template exists");
    }
    let templates = list_templates(user_dir);
    match templates.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
        Some(template) => template.clone(),
        None => {
            tracing::warn!(template = %name, "unknown minutes template; falling back to default");
            templates
                .into_iter()
                .next()
                .expect("the default built-in template exists")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_template() {
        let t = parse_template(
            "---\nname: standup\ndescription: 每日站会\nlanguage: zh\n---\n\n- 要点一\n- 要点二\n",
        )
        .unwrap();
        assert_eq!(t.name, "standup");
        assert_eq!(t.description, "每日站会");
        assert_eq!(t.language.as_deref(), Some("zh"));
        assert_eq!(t.body, "- 要点一\n- 要点二");
        assert!(!t.builtin);
    }

    #[test]
    fn tolerates_unknown_keys_and_quoted_values() {
        let t = parse_template("---\nname: \"weekly\"\nauthor: someone\n---\nbody\n").unwrap();
        assert_eq!(t.name, "weekly");
        assert_eq!(t.description, "");
        assert_eq!(t.language, None);
        assert_eq!(t.body, "body");
    }

    #[test]
    fn rejects_missing_fences_and_missing_name() {
        assert_eq!(
            parse_template("no frontmatter at all"),
            Err(TemplateError::NoFrontmatter)
        );
        assert_eq!(
            parse_template("---\nname: x\n"),
            Err(TemplateError::UnclosedFrontmatter)
        );
        assert_eq!(
            parse_template("---\ndescription: 没有名字\n---\nbody"),
            Err(TemplateError::MissingName)
        );
        assert!(matches!(
            parse_template("---\nname x\n---\nbody"),
            Err(TemplateError::MalformedFrontmatterLine(_))
        ));
    }

    #[test]
    fn builtins_are_valid_and_default_is_first_with_empty_body() {
        let builtins = builtin_templates();
        assert!(builtins.len() >= 4);
        assert_eq!(builtins[0].name, DEFAULT_TEMPLATE_NAME);
        assert!(builtins[0].body.is_empty());
        for name in ["action-items", "decision-log", "write-email"] {
            assert!(
                builtins.iter().any(|t| t.name == name),
                "missing built-in template {name}"
            );
        }
        assert!(builtins.iter().all(|t| t.builtin));
    }

    #[test]
    fn user_templates_override_builtins_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("action-items.md"),
            "---\nname: action-items\ndescription: 用户自定义\n---\n- 自定义说明\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("standup.md"),
            "---\nname: standup\ndescription: 站会\n---\n- 站会说明\n",
        )
        .unwrap();

        let templates = list_templates(Some(dir.path()));
        let action_items = templates.iter().find(|t| t.name == "action-items").unwrap();
        assert_eq!(action_items.description, "用户自定义");
        assert_eq!(action_items.body, "- 自定义说明");
        assert!(!action_items.builtin);
        // The extra user template is appended; untouched built-ins remain.
        assert!(templates.iter().any(|t| t.name == "standup"));
        assert!(templates
            .iter()
            .any(|t| t.name == "decision-log" && t.builtin));
    }

    #[test]
    fn malformed_user_templates_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.md"), "not a template").unwrap();
        std::fs::write(
            dir.path().join("good.md"),
            "---\nname: good\ndescription: ok\n---\nbody\n",
        )
        .unwrap();
        // Non-.md files are not templates at all.
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let templates = load_user_templates(dir.path());
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "good");
        // A missing directory is simply empty.
        assert!(load_user_templates(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn resolve_falls_back_to_default_for_empty_and_unknown_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_template("", None).name, DEFAULT_TEMPLATE_NAME);
        assert_eq!(resolve_template("  ", None).name, DEFAULT_TEMPLATE_NAME);
        assert_eq!(
            resolve_template("does-not-exist", Some(dir.path())).name,
            DEFAULT_TEMPLATE_NAME
        );
        // Name matching is case-insensitive.
        assert_eq!(resolve_template("Action-Items", None).name, "action-items");
    }
}
