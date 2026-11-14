//! Official Codex catalog instructions bundled at release time.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use hofund_plugin_sdk::json::{self, Value};

mod prompt;

/// Unknown models and routing groups use Codex's `prompt.md` text, kept as an
/// ASCII string literal in `prompt.rs` (byte for byte the upstream file).
pub(crate) fn unknown_model_prompt() -> &'static str {
    prompt::PROMPT
}

const BUNDLED_JSON: &str = include_str!("codex_instructions.json");

#[derive(Debug, Clone)]
struct BundledEntry {
    slug: String,
    instructions_template: String,
}

#[derive(Debug, Default)]
struct Bundled {
    entries: Vec<BundledEntry>,
}

static BUNDLED: OnceLock<Bundled> = OnceLock::new();

fn bundled() -> &'static Bundled {
    BUNDLED.get_or_init(|| {
        let value: Value = json::from_str(BUNDLED_JSON).expect("bundled instructions json");
        let mut entries = Vec::new();
        for row in value
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(slug) = row.get("slug").and_then(Value::as_str) else {
                continue;
            };
            let Some(template) = row.get("instructions_template").and_then(Value::as_str) else {
                continue;
            };
            entries.push(BundledEntry {
                slug: slug.to_owned(),
                instructions_template: template.to_owned(),
            });
        }
        Bundled { entries }
    })
}

/// Whether `slug` names a routing group in hofund's model list.
pub(crate) fn is_routing_group(id: &str) -> bool {
    id.starts_with("group/")
}

/// The last `/` segment of a model id, or the whole id when there is no slash.
pub(crate) fn model_slug(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// Longest bundled slug that matches `slug` the way Codex 0.166 does.
fn longest_slug_match<'a>(slug: &str, entries: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    entries
        .filter(|entry| slug_matches(entry, slug))
        .max_by_key(|entry| entry.len())
}

fn slug_matches(entry_slug: &str, slug: &str) -> bool {
    slug == entry_slug
        || slug
            .strip_prefix(entry_slug)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// Instructions chosen for a catalog model id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelInstructions {
    /// From the bundled catalog: template only, as `base_instructions`.
    Bundled { base_instructions: String },
    /// From the user's Codex home cache: fields copied as sent.
    Cached {
        base_instructions: String,
        model_messages: Option<Value>,
    },
    /// Codex's prompt for an unknown model or a routing group.
    UnknownPrompt,
}

impl ModelInstructions {
    /// Applies these instructions onto a catalog entry object.
    pub(crate) fn apply(self, entry: &mut Value) {
        let object = entry.as_object_mut().expect("catalog entry object");
        match self {
            Self::Bundled { base_instructions } => {
                object.insert(
                    "base_instructions".to_owned(),
                    Value::String(base_instructions),
                );
            }
            Self::Cached {
                base_instructions,
                model_messages,
            } => {
                object.insert(
                    "base_instructions".to_owned(),
                    Value::String(base_instructions),
                );
                if let Some(model_messages) = model_messages {
                    object.insert("model_messages".to_owned(), model_messages);
                }
            }
            Self::UnknownPrompt => {
                object.insert(
                    "base_instructions".to_owned(),
                    Value::String(unknown_model_prompt().to_owned()),
                );
            }
        }
    }
}

/// Resolves instructions for a catalog model id.
pub(crate) struct Lookup {
    cache: Option<CacheFile>,
}

impl Lookup {
    /// Reads `models_cache.json` under [`codex_home`] when present.
    pub(crate) fn with_codex_home() -> Self {
        Self {
            cache: codex_home()
                .map(|home| home.join("models_cache.json"))
                .and_then(|path| CacheFile::read(&path).ok()),
        }
    }

    #[cfg(test)]
    pub(crate) fn without_cache() -> Self {
        Self { cache: None }
    }

    /// The instructions Codex should see for gateway model `id`.
    pub(crate) fn for_id(&self, id: &str) -> ModelInstructions {
        if is_routing_group(id) {
            return ModelInstructions::UnknownPrompt;
        }
        let slug = model_slug(id);
        if let Some(cache) = &self.cache
            && let Some(hit) = cache.match_slug(slug)
        {
            return hit;
        }
        let entries = &bundled().entries;
        if let Some(found) =
            longest_slug_match(slug, entries.iter().map(|entry| entry.slug.as_str()))
            && let Some(entry) = entries.iter().find(|entry| entry.slug == found)
        {
            return ModelInstructions::Bundled {
                base_instructions: entry.instructions_template.clone(),
            };
        }
        ModelInstructions::UnknownPrompt
    }
}

struct CacheFile {
    slugs: Vec<CacheRow>,
}

struct CacheRow {
    slug: String,
    base_instructions: String,
    model_messages: Option<Value>,
}

impl CacheFile {
    fn match_slug(&self, slug: &str) -> Option<ModelInstructions> {
        let entry_slug = longest_slug_match(slug, self.slugs.iter().map(|row| row.slug.as_str()))?;
        let row = self.slugs.iter().find(|row| row.slug == entry_slug)?;
        Some(ModelInstructions::Cached {
            base_instructions: row.base_instructions.clone(),
            model_messages: row.model_messages.clone(),
        })
    }
}

impl CacheFile {
    fn read(path: &Path) -> Result<Self, ()> {
        let bytes = std::fs::read(path).map_err(|_| ())?;
        let value: Value = json::from_slice(&bytes).map_err(|_| ())?;
        let mut slugs = Vec::new();
        for row in value
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(slug) = row.get("slug").and_then(Value::as_str) else {
                continue;
            };
            let Some(base) = row
                .get("base_instructions")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            let model_messages = row
                .get("model_messages")
                .filter(|value| !value.is_null())
                .cloned();
            slugs.push(CacheRow {
                slug: slug.to_owned(),
                base_instructions: base,
                model_messages,
            });
        }
        if slugs.is_empty() {
            return Err(());
        }
        Ok(Self { slugs })
    }
}

/// Codex's config home (`CODEX_HOME`, or `~/.codex` when unset).
pub(crate) fn codex_home_dir() -> Option<PathBuf> {
    codex_home()
}

fn codex_home() -> Option<PathBuf> {
    // Tests never read the real Codex home of the machine they run on.
    if cfg!(test) {
        return test_codex_home();
    }
    match std::env::var("CODEX_HOME") {
        Ok(home) if !home.is_empty() => Some(PathBuf::from(home)),
        _ => default_codex_home(),
    }
}

fn default_codex_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".codex"))
}

#[cfg(test)]
mod test_home {
    use std::cell::RefCell;
    use std::path::PathBuf;

    // Per thread, so a test that sets a home cannot leak it into another
    // test running in parallel.
    thread_local! {
        static HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub(super) fn get() -> Option<PathBuf> {
        HOME.with(|home| home.borrow().clone())
    }

    pub(super) fn set(home: Option<PathBuf>) {
        HOME.with(|slot| *slot.borrow_mut() = home);
    }
}

fn test_codex_home() -> Option<PathBuf> {
    #[cfg(test)]
    return test_home::get();
    #[cfg(not(test))]
    None
}

#[cfg(test)]
pub(crate) fn set_test_codex_home(home: Option<PathBuf>) {
    test_home::set(home);
}

/// Whether Codex at `client_version` reads a whole catalog without the 1 MiB cap.
pub(crate) fn reads_whole_catalog(client_version: Option<&str>) -> bool {
    let Some(raw) = client_version else {
        return false;
    };
    let Some(parsed) = parse_version(raw) else {
        return false;
    };
    parsed >= (0, 166, 0)
}

fn parse_version(raw: &str) -> Option<(u32, u32, u32)> {
    let head = raw.split(['-', '+']).next().unwrap_or(raw);
    let mut parts = head.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod sync_tests;
