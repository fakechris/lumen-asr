//! The checked-in clip set (`data/asr-corpus.json`) with per-clip
//! expectations, plus long clips assembled from named sources.
//!
//! Design rules:
//! - every clip is our own sentence; audio is synthesized locally
//!   (`eval synth`, macOS `say`) so nothing but text ships in the repo;
//! - `text` is what was spoken (the ASR reference), `clean` is what a
//!   perfect cleanup should produce from `text`;
//! - `must` phrases must survive (normalized substring), `must_not`
//!   phrases must be gone after cleanup — these drive the cleanup checks;
//! - `terms` are dictionary entries the clip exercises; term hits count
//!   exact (normalized) appearances in the hypothesis;
//! - `long` clips have no text of their own: they name `source_clip_ids`
//!   joined with `gap_seconds` of silence, and their expectations are the
//!   union of the sources'.

use std::collections::HashMap;

pub const BASE_CORPUS_JSON: &str = include_str!("../data/asr-corpus.json");
pub const BASELINES_JSON: &str = include_str!("../data/baselines.json");

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum EvalSet {
    #[serde(rename = "zh-filler")]
    ZhFiller,
    #[serde(rename = "zh-plain")]
    ZhPlain,
    #[serde(rename = "en")]
    En,
    #[serde(rename = "mix")]
    Mix,
    #[serde(rename = "dict")]
    Dict,
    #[serde(rename = "long")]
    Long,
}

impl EvalSet {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvalSet::ZhFiller => "zh-filler",
            EvalSet::ZhPlain => "zh-plain",
            EvalSet::En => "en",
            EvalSet::Mix => "mix",
            EvalSet::Dict => "dict",
            EvalSet::Long => "long",
        }
    }

    pub fn parse(value: &str) -> Option<EvalSet> {
        Some(match value {
            "zh-filler" => EvalSet::ZhFiller,
            "zh-plain" => EvalSet::ZhPlain,
            "en" => EvalSet::En,
            "mix" => EvalSet::Mix,
            "dict" => EvalSet::Dict,
            "long" => EvalSet::Long,
            _ => return None,
        })
    }

    pub const ALL: [EvalSet; 6] = [
        EvalSet::ZhFiller,
        EvalSet::ZhPlain,
        EvalSet::En,
        EvalSet::Mix,
        EvalSet::Dict,
        EvalSet::Long,
    ];
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvalClip {
    pub id: String,
    pub set: EvalSet,
    /// Spoken-script dominant language, used for TTS voice selection and
    /// as the engine locale hint: "zh", "en", or "mix" (treated as zh).
    pub lang: String,
    /// What was spoken; `None` for long clips (see `source_clip_ids`).
    #[serde(default)]
    pub text: Option<String>,
    /// What a perfect cleanup produces from `text`; `None` for long clips.
    #[serde(default)]
    pub clean: Option<String>,
    #[serde(default)]
    pub must: Vec<String>,
    #[serde(default)]
    pub must_not: Vec<String>,
    #[serde(default)]
    pub terms: Vec<String>,
    /// Expected speaker count for meeting-style clips; drives the
    /// diarization-collapse guard when present.
    #[serde(default)]
    pub speakers: Option<u32>,
    #[serde(default)]
    pub source_clip_ids: Vec<String>,
    #[serde(default)]
    pub gap_seconds: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Corpus {
    pub version: u32,
    #[serde(default)]
    pub dictionary_terms: Vec<String>,
    pub clips: Vec<EvalClip>,
}

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("corpus json invalid: {0}")]
    InvalidJson(String),
    #[error("corpus invalid: {0}")]
    Invalid(String),
}

impl Corpus {
    /// The corpus checked into the repository.
    pub fn embedded() -> Result<Corpus, CorpusError> {
        Corpus::from_json(BASE_CORPUS_JSON)
    }

    pub fn from_json(json: &str) -> Result<Corpus, CorpusError> {
        let corpus: Corpus =
            serde_json::from_str(json).map_err(|e| CorpusError::InvalidJson(e.to_string()))?;
        corpus.validate()?;
        Ok(corpus)
    }

    /// Resolve a clip's spoken text and expected cleanup, assembling long
    /// clips from their sources on the fly.
    pub fn resolved_text(&self, clip: &EvalClip) -> Result<(String, String), CorpusError> {
        if clip.source_clip_ids.is_empty() {
            let text = clip.text.clone().ok_or_else(|| {
                CorpusError::Invalid(format!("clip {} has neither text nor sources", clip.id))
            })?;
            let clean = clip.clean.clone().unwrap_or_else(|| text.clone());
            Ok((text, clean))
        } else {
            let mut text_parts = Vec::new();
            let mut clean_parts = Vec::new();
            for source_id in &clip.source_clip_ids {
                let source = self.clip(source_id).ok_or_else(|| {
                    CorpusError::Invalid(format!(
                        "long clip {} references unknown source {}",
                        clip.id, source_id
                    ))
                })?;
                let (t, c) = self.resolved_text(source)?;
                text_parts.push(t);
                clean_parts.push(c);
            }
            Ok((text_parts.join(" "), clean_parts.join(" ")))
        }
    }

    /// Long clips inherit the union of their sources' expectations.
    pub fn resolved_must(&self, clip: &EvalClip) -> Vec<String> {
        if clip.source_clip_ids.is_empty() {
            return clip.must.clone();
        }
        let mut merged: Vec<String> = clip.must.clone();
        for source_id in &clip.source_clip_ids {
            if let Some(source) = self.clip(source_id) {
                merged.extend(source.must.iter().cloned());
            }
        }
        merged
    }

    pub fn resolved_terms(&self, clip: &EvalClip) -> Vec<String> {
        if clip.source_clip_ids.is_empty() {
            return clip.terms.clone();
        }
        let mut merged: Vec<String> = clip.terms.clone();
        for source_id in &clip.source_clip_ids {
            if let Some(source) = self.clip(source_id) {
                merged.extend(source.terms.iter().cloned());
            }
        }
        merged
    }

    pub fn clip(&self, id: &str) -> Option<&EvalClip> {
        self.clips.iter().find(|c| c.id == id)
    }

    /// Clips that carry audio of their own (everything except long).
    pub fn base_clips(&self) -> impl Iterator<Item = &EvalClip> {
        self.clips.iter().filter(|c| c.source_clip_ids.is_empty())
    }

    fn validate(&self) -> Result<(), CorpusError> {
        let mut seen: HashMap<&str, ()> = HashMap::new();
        for clip in &self.clips {
            if clip.id.is_empty() {
                return Err(CorpusError::Invalid("clip with empty id".into()));
            }
            if seen.insert(clip.id.as_str(), ()).is_some() {
                return Err(CorpusError::Invalid(format!(
                    "duplicate clip id {}",
                    clip.id
                )));
            }
            if clip.source_clip_ids.is_empty() {
                if clip.text.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(CorpusError::Invalid(format!(
                        "clip {} has empty text",
                        clip.id
                    )));
                }
            } else {
                // Long clips reference base clips only; text comes from sources.
                if clip.source_clip_ids.iter().any(|s| {
                    self.clip(s)
                        .map(|c| !c.source_clip_ids.is_empty())
                        .unwrap_or(true)
                }) {
                    return Err(CorpusError::Invalid(format!(
                        "long clip {} must reference existing base clips",
                        clip.id
                    )));
                }
                if clip.gap_seconds.unwrap_or(0.0) < 0.0 {
                    return Err(CorpusError::Invalid(format!(
                        "clip {} has negative gap",
                        clip.id
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_corpus_loads_and_meets_minimums() {
        let corpus = Corpus::embedded().expect("embedded corpus parses");
        assert!(
            corpus.clips.len() >= 23,
            "expected >= 23 clips (21 base + 2 long)"
        );
        let per_set: HashMap<EvalSet, usize> = {
            let mut m: HashMap<EvalSet, usize> = HashMap::new();
            for clip in &corpus.clips {
                *m.entry(clip.set).or_default() += 1;
            }
            m
        };
        for set in EvalSet::ALL {
            assert!(
                per_set.get(&set).copied().unwrap_or(0) >= 2,
                "set {} has too few clips",
                set.as_str()
            );
        }
        assert!(!corpus.dictionary_terms.is_empty());
    }

    #[test]
    fn long_clips_resolve_from_sources() {
        let corpus = Corpus::embedded().unwrap();
        let long = corpus.clip("long-mix-zh").expect("long clip exists");
        let (text, clean) = corpus.resolved_text(long).unwrap();
        assert!(
            text.contains("讨论一下预算"),
            "source text is joined: {text}"
        );
        assert!(
            clean.contains("我们下周三下午三点开个会"),
            "source clean is joined: {clean}"
        );
        let must = corpus.resolved_must(long);
        assert!(
            must.contains(&"下周三".to_string()),
            "source must phrases are inherited"
        );
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let json = r#"{"version":1,"dictionary_terms":[],"clips":[
            {"id":"a","set":"en","lang":"en","text":"hello","clean":"hello","must":[],"must_not":[],"terms":[]},
            {"id":"a","set":"en","lang":"en","text":"world","clean":"world","must":[],"must_not":[],"terms":[]}
        ]}"#;
        let err = Corpus::from_json(json).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn long_clip_referencing_long_clip_is_rejected() {
        let json = r#"{"version":1,"dictionary_terms":[],"clips":[
            {"id":"a","set":"en","lang":"en","text":"hello","clean":"hello","must":[],"must_not":[],"terms":[]},
            {"id":"long1","set":"long","lang":"en","must":[],"must_not":[],"terms":[],"source_clip_ids":["long2"],"gap_seconds":0.8},
            {"id":"long2","set":"long","lang":"en","must":[],"must_not":[],"terms":[],"source_clip_ids":["a"],"gap_seconds":0.8}
        ]}"#;
        let err = Corpus::from_json(json).unwrap_err();
        assert!(err.to_string().contains("base clips"), "{err}");
    }
}
