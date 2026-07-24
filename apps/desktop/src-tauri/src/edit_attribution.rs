//! Pure attribution of a later field value to one inserted dictation span.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditProjection {
    Unchanged,
    Edited { after_text: String },
    Unrelated,
}

#[derive(Debug, Clone)]
pub struct InsertionAnchor {
    inserted_text: String,
    left_context: String,
    right_context: String,
}

/// Pins one uniquely rendered terminal row without requiring the rest of the
/// pane to remain unchanged while a TUI redraws status/header/footer content.
#[derive(Debug, Clone)]
pub struct TerminalInsertionAnchor {
    inserted_text: String,
    left_context: String,
    right_context: String,
}

impl TerminalInsertionAnchor {
    pub fn from_snapshot(snapshot: &str, inserted_text: &str) -> Result<Self, String> {
        if inserted_text.is_empty() {
            return Err("inserted_text_empty".into());
        }
        if inserted_text.contains('\n') || inserted_text.contains('\r') {
            return Err("pane_multiline_insert_unsupported".into());
        }
        let mut matches = snapshot.match_indices(inserted_text);
        let Some((start, _)) = matches.next() else {
            return Err("inserted_text_not_found_in_pane".into());
        };
        if matches.next().is_some() {
            return Err("inserted_text_not_unique_in_pane".into());
        }
        let end = start + inserted_text.len();
        let line_start = snapshot[..start]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or_default();
        let line_end = snapshot[end..]
            .find('\n')
            .map(|offset| end + offset)
            .unwrap_or(snapshot.len());
        let left_context = snapshot[line_start..start].to_owned();
        let right_context = snapshot[end..line_end].to_owned();
        if left_context.is_empty() && right_context.is_empty() {
            return Err("pane_line_context_unavailable".into());
        }
        Ok(Self {
            inserted_text: inserted_text.to_owned(),
            left_context,
            right_context,
        })
    }

    pub fn project(&self, snapshot: &str) -> EditProjection {
        let lines = snapshot.split('\n').collect::<Vec<_>>();
        let mut candidates = lines
            .iter()
            .copied()
            .filter(|line| self.project_line(line).is_some());
        if let Some(line) = candidates.next() {
            if candidates.next().is_none() {
                return self.projection_for_line(line);
            }
        }
        EditProjection::Unrelated
    }

    fn projection_for_line(&self, line: &str) -> EditProjection {
        let Some(after_text) = self.project_line(line) else {
            return EditProjection::Unrelated;
        };
        if after_text == self.inserted_text {
            EditProjection::Unchanged
        } else {
            EditProjection::Edited { after_text }
        }
    }

    fn project_line(&self, line: &str) -> Option<String> {
        if !line.starts_with(&self.left_context)
            || !line.ends_with(&self.right_context)
            || line.len() < self.left_context.len() + self.right_context.len()
        {
            return None;
        }
        let end = line.len() - self.right_context.len();
        let value = &line[self.left_context.len()..end];
        if value.chars().count()
            > self
                .inserted_text
                .chars()
                .count()
                .saturating_mul(4)
                .saturating_add(256)
        {
            return None;
        }
        Some(value.to_owned())
    }
}

impl InsertionAnchor {
    pub fn from_post_insert(field_value: &str, inserted_text: &str) -> Result<Self, String> {
        if inserted_text.is_empty() {
            return Err("inserted_text_empty".into());
        }
        let mut matches = field_value.match_indices(inserted_text);
        let Some((start, _)) = matches.next() else {
            return Err("inserted_text_not_found_in_field".into());
        };
        if matches.next().is_some() {
            return Err("inserted_text_not_unique_in_field".into());
        }
        let end = start + inserted_text.len();
        Ok(Self {
            inserted_text: inserted_text.to_owned(),
            left_context: field_value[..start].to_owned(),
            right_context: field_value[end..].to_owned(),
        })
    }

    pub fn project(&self, field_value: &str) -> EditProjection {
        if !field_value.starts_with(&self.left_context)
            || !field_value.ends_with(&self.right_context)
            || field_value.len() < self.left_context.len() + self.right_context.len()
        {
            return EditProjection::Unrelated;
        }
        let after =
            &field_value[self.left_context.len()..field_value.len() - self.right_context.len()];
        if after == self.inserted_text {
            EditProjection::Unchanged
        } else {
            EditProjection::Edited {
                after_text: after.to_owned(),
            }
        }
    }

    /// Terminal accessibility trees may represent an in-place command-line redraw by appending
    /// the redrawn prompt and line to AXValue instead of replacing the old line. In that case,
    /// attribute only the text after the last exact copy of the original prompt prefix.
    pub fn project_terminal_current_line(&self, field_value: &str) -> EditProjection {
        if !self.right_context.is_empty() {
            return self.project(field_value);
        }
        let prompt_prefix = self
            .left_context
            .rsplit_once('\n')
            .map(|(_, line)| line)
            .unwrap_or(self.left_context.as_str());
        if prompt_prefix.is_empty() {
            return collapse_terminal_redraw(self.project(field_value), &self.inserted_text);
        }
        let Some((start, _)) = field_value.rmatch_indices(prompt_prefix).next() else {
            return EditProjection::Unrelated;
        };
        let after = &field_value[start + prompt_prefix.len()..];
        if after.contains('\n')
            || after.chars().count()
                > self
                    .inserted_text
                    .chars()
                    .count()
                    .saturating_mul(4)
                    .saturating_add(256)
        {
            return EditProjection::Unrelated;
        }
        let projection = if after == self.inserted_text {
            EditProjection::Unchanged
        } else {
            EditProjection::Edited {
                after_text: after.to_owned(),
            }
        };
        collapse_terminal_redraw(projection, &self.inserted_text)
    }
}

fn collapse_terminal_redraw(projection: EditProjection, inserted_text: &str) -> EditProjection {
    let EditProjection::Edited { after_text } = projection else {
        return projection;
    };
    let after_text = after_text.trim_end_matches([' ', '\u{00a0}']).to_owned();
    let chars = after_text.chars().collect::<Vec<_>>();
    let minimum = inserted_text.chars().count().saturating_div(3).max(3);
    let maximum = inserted_text
        .chars()
        .count()
        .saturating_mul(4)
        .saturating_add(256)
        .min(chars.len() / 2);
    for length in (minimum..=maximum).rev() {
        if chars[..length] != chars[chars.len() - length..] {
            continue;
        }
        let middle = &chars[length..chars.len() - length];
        let has_terminal_clear = middle
            .split(|ch| !ch.is_whitespace())
            .any(|run| run.len() >= 8);
        if has_terminal_clear {
            return EditProjection::Edited {
                after_text: chars[..length].iter().collect(),
            };
        }
    }
    if let Some(after_text) = closest_terminal_suffix(&chars, inserted_text) {
        return EditProjection::Edited { after_text };
    }
    if after_text.contains('\n')
        || chars.len()
            > inserted_text
                .chars()
                .count()
                .saturating_mul(2)
                .saturating_add(64)
    {
        return EditProjection::Unrelated;
    }
    EditProjection::Edited { after_text }
}

fn closest_terminal_suffix(field_tail: &[char], inserted_text: &str) -> Option<String> {
    let inserted = inserted_text.chars().collect::<Vec<_>>();
    if inserted.is_empty()
        || field_tail.len() > inserted.len().saturating_mul(4).saturating_add(256)
    {
        return None;
    }
    let mut previous = (0..=field_tail.len())
        .map(|start| (0_usize, start))
        .collect::<Vec<_>>();
    for row in 1..=inserted.len() {
        let mut current = vec![(0_usize, 0_usize); field_tail.len() + 1];
        current[0] = (row, 0);
        for column in 1..=field_tail.len() {
            let substitutions =
                previous[column - 1].0 + usize::from(inserted[row - 1] != field_tail[column - 1]);
            let candidates = [
                (previous[column].0 + 1, previous[column].1),
                (current[column - 1].0 + 1, current[column - 1].1),
                (substitutions, previous[column - 1].1),
            ];
            current[column] = candidates
                .into_iter()
                .min_by_key(|(cost, start)| {
                    let candidate_len = column.saturating_sub(*start);
                    (*cost, candidate_len.abs_diff(row))
                })
                .expect("three edit-distance candidates");
        }
        previous = current;
    }
    let (distance, start) = previous[field_tail.len()];
    let candidate = &field_tail[start..];
    if start < 8
        || candidate.is_empty()
        || candidate.contains(&'\n')
        || candidate.len() > inserted.len().saturating_mul(2).saturating_add(64)
        || distance.saturating_mul(100) > inserted.len().max(candidate.len()).saturating_mul(35)
    {
        return None;
    }
    Some(candidate.iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_pane_anchor_ignores_unrelated_screen_repaints() {
        let anchor = TerminalInsertionAnchor::from_snapshot(
            "status: listening\nproject $ HERDR\nfooter: old",
            "HERDR",
        )
        .unwrap();

        assert_eq!(
            anchor.project("status: ready\nproject $ Herdr\nfooter: new"),
            EditProjection::Edited {
                after_text: "Herdr".into()
            }
        );
    }

    #[test]
    fn terminal_pane_anchor_rejects_a_context_free_rendered_row() {
        let error =
            TerminalInsertionAnchor::from_snapshot("header\nHERDR\nfooter", "HERDR").unwrap_err();

        assert_eq!(error, "pane_line_context_unavailable");
    }

    #[test]
    fn terminal_pane_anchor_rejects_multiple_later_context_matches() {
        let anchor =
            TerminalInsertionAnchor::from_snapshot("header\nproject $ HERDR\nfooter", "HERDR")
                .unwrap();

        assert_eq!(
            anchor.project("project $ Herdr\nproject $ Other"),
            EditProjection::Unrelated
        );
    }

    #[test]
    fn terminal_pane_anchor_rejects_an_ambiguous_initial_screen() {
        let error =
            TerminalInsertionAnchor::from_snapshot("HERDR\nother HERDR", "HERDR").unwrap_err();

        assert_eq!(error, "inserted_text_not_unique_in_pane");
    }

    #[test]
    fn terminal_pane_anchor_rejects_multiline_insertions_for_ax_fallback() {
        let error = TerminalInsertionAnchor::from_snapshot(
            "project $ first line\nsecond line",
            "first line\nsecond line",
        )
        .unwrap_err();

        assert_eq!(error, "pane_multiline_insert_unsupported");
    }

    #[test]
    fn extracts_an_edit_to_the_inserted_span_without_storing_surrounding_document_text() {
        let anchor = InsertionAnchor::from_post_insert("前文 Lumen Asr 后文", "Lumen Asr").unwrap();

        assert_eq!(
            anchor.project("前文 Lumen ASR 后文"),
            EditProjection::Edited {
                after_text: "Lumen ASR".into()
            }
        );
    }

    #[test]
    fn accepts_deleting_the_inserted_span() {
        let anchor = InsertionAnchor::from_post_insert("prefix听写结果", "听写结果").unwrap();

        assert_eq!(
            anchor.project("prefix"),
            EditProjection::Edited {
                after_text: String::new()
            }
        );
    }

    #[test]
    fn rejects_an_edit_outside_the_inserted_span() {
        let anchor = InsertionAnchor::from_post_insert("旧内容 本次听写 尾部", "本次听写").unwrap();

        assert_eq!(
            anchor.project("旧内容已改 本次听写 尾部"),
            EditProjection::Unrelated
        );
    }

    #[test]
    fn rejects_an_ambiguous_duplicate_insertion() {
        let error = InsertionAnchor::from_post_insert("Codex and Codex", "Codex").unwrap_err();

        assert_eq!(error, "inserted_text_not_unique_in_field");
    }

    #[test]
    fn reports_the_original_field_as_unchanged() {
        let anchor = InsertionAnchor::from_post_insert("一段听写", "一段听写").unwrap();

        assert_eq!(anchor.project("一段听写"), EditProjection::Unchanged);
    }

    #[test]
    fn extracts_only_the_latest_terminal_line_after_an_ax_redraw() {
        let anchor = InsertionAnchor::from_post_insert(
            "history\nproject $ LUMEN ORIGINAL",
            "LUMEN ORIGINAL",
        )
        .unwrap();

        assert_eq!(
            anchor.project_terminal_current_line(
                "history\nproject $ LUMEN EDITED%              project $ LUMEN EDITED"
            ),
            EditProjection::Edited {
                after_text: "LUMEN EDITED".into()
            }
        );
    }

    #[test]
    fn collapses_a_terminal_redraw_when_the_prompt_was_not_exposed_initially() {
        let anchor =
            InsertionAnchor::from_post_insert("last login\nLUMEN ORIGINAL", "LUMEN ORIGINAL")
                .unwrap();

        assert_eq!(
            anchor.project_terminal_current_line(
                "last login\nLUMEN EDITED%              project $ LUMEN EDITED"
            ),
            EditProjection::Edited {
                after_text: "LUMEN EDITED".into()
            }
        );
    }

    #[test]
    fn strips_a_redrawn_shell_prompt_from_a_similar_edited_suffix() {
        let anchor = InsertionAnchor::from_post_insert(
            "last login\nLUMEN ORIGINAL 1234567890",
            "LUMEN ORIGINAL 1234567890",
        )
        .unwrap();

        assert_eq!(
            anchor.project_terminal_current_line(
                "last login\nproject on main via rust LUMEN EDITED 1234567890"
            ),
            EditProjection::Edited {
                after_text: "LUMEN EDITED 1234567890".into()
            }
        );
    }

    #[test]
    fn rejects_terminal_command_execution_as_an_edit() {
        let anchor =
            InsertionAnchor::from_post_insert("last login\nLUMEN ORIGINAL", "LUMEN ORIGINAL")
                .unwrap();

        assert_eq!(
            anchor.project_terminal_current_line(
                "last login\nLUMEN ORIGINAL\ncommand output\nproject $ "
            ),
            EditProjection::Unrelated
        );
    }
}
