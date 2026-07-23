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
}
