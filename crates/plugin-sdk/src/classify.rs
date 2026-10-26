//! Wire-independent classification of model-list rows.

const NON_CHAT: &[&str] = &[
    "embed",
    "-tts",
    "image",
    "audio",
    "-live",
    "robotics",
    "computer-use",
    "deep-research",
    "transcribe",
    "realtime",
    "moderation",
    "whisper",
    "dall-e",
    "sora",
];

const DRAWING_IDS: &[&str] = &[
    "image",
    "imagen",
    "imagine",
    "dall-e",
    "flux",
    "seedream",
    "cogview",
    "stable-diffusion",
    "sdxl",
    "wanx",
    "kolors",
    "hidream",
    "banana",
];

/// Non-chat capabilities identified on a model-list row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelTags {
    draws: bool,
    films: bool,
    decides: bool,
}

impl ModelTags {
    /// Whether the model draws images.
    #[must_use]
    pub const fn draws(self) -> bool {
        self.draws
    }

    /// Whether the model makes films.
    #[must_use]
    pub const fn films(self) -> bool {
        self.films
    }

    /// Whether the model is a decision model.
    #[must_use]
    pub const fn decides(self) -> bool {
        self.decides
    }
}

/// Classifies one live-list row, or returns `None` when it is not a chat model.
///
/// The id substring check is case-sensitive. Drawing, film, and decision rows
/// survive that check so a host can expose them on their own surfaces.
#[must_use]
pub fn classify_model(id: &str, kind: &str) -> Option<ModelTags> {
    let decides = kind == "decision";
    let films = kind == "video";
    let lower = id.to_ascii_lowercase();
    let draws = !decides
        && !films
        && !lower.contains("deep-research")
        && (kind == "image" || DRAWING_IDS.iter().any(|part| lower.contains(part)));
    if !decides && !films && !draws && NON_CHAT.iter().any(|part| id.contains(part)) {
        return None;
    }
    Some(ModelTags {
        draws,
        films,
        decides,
    })
}

#[cfg(test)]
mod tests {
    use super::classify_model;

    #[test]
    fn non_chat_ids_are_dropped_but_special_rows_survive() {
        assert!(classify_model("text-embedding-3-small", "").is_none());
        assert!(classify_model("gpt-4o-mini-tts", "").is_none());
        assert!(classify_model("tts-1", "").is_some());
        assert!(classify_model("gpt-image-2", "").expect("drawing").draws());
        assert!(classify_model("dall-e-3", "").expect("drawing").draws());
        assert!(
            classify_model("flux-kontext-pro", "")
                .expect("drawing")
                .draws()
        );
        assert!(classify_model("sora-2", "video").expect("film").films());
        assert!(classify_model("deep-research-image", "").is_none());
        assert!(
            classify_model("router", "decision")
                .expect("decision")
                .decides()
        );
    }
}
