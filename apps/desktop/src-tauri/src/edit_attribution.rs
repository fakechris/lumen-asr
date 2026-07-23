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
