//! Last positive prompt and cache counts for one streamed reply.
//!
//! Chat `prompt_tokens` and Generative Language `promptTokenCount` include
//! tokens served from the cache and tokens written into it. A later chunk may
//! repeat the prompt without the cache, or name the cache beside a prompt of
//! zero. Each count kept here is the last positive value that reply named.
//! A zero, or a field the chunk omits, does not drop a positive count already
//! seen. The prompt stays the whole count the protocol reported. It is not
//! reduced by the cache.

use hofund_ir::Usage;

/// Prompt, cache read, and cache write last named with a positive count.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PromptCount {
    prompt: u64,
    read: u64,
    write: u64,
}

impl PromptCount {
    /// Restores any positive prompt or cache count this reply already named.
    ///
    /// When the chunk named a prompt smaller than the one already kept, and it
    /// also named a total, that total grows by the restored prompt. A chunk
    /// that omits the prompt leaves its total alone.
    pub(crate) fn fit(&mut self, usage: &mut Usage) {
        let named_prompt = usage.input_tokens;
        remember(&mut self.prompt, &mut usage.input_tokens);
        remember(&mut self.read, &mut usage.cache_read_tokens);
        remember(&mut self.write, &mut usage.cache_write_tokens);
        let Some(named) = named_prompt else {
            return;
        };
        let restored = usage.input_tokens.unwrap_or(0).saturating_sub(named);
        if restored > 0
            && let Some(total) = usage.total_tokens.as_mut()
        {
            *total = total.saturating_add(restored);
        }
    }
}

fn remember(kept: &mut u64, value: &mut Option<u64>) {
    match *value {
        Some(count) if count > 0 => *kept = count,
        _ if *kept > 0 => *value = Some(*kept),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::PromptCount;
    use hofund_ir::Usage;

    #[test]
    fn d280_fit_keeps_the_last_positive_counts() {
        let mut count = PromptCount::default();
        let mut first = Usage {
            input_tokens: Some(2006),
            output_tokens: Some(0),
            total_tokens: Some(2006),
            cache_read_tokens: Some(1920),
            cache_write_tokens: Some(12),
            ..Usage::default()
        };
        count.fit(&mut first);
        let mut later = Usage {
            input_tokens: Some(0),
            output_tokens: Some(300),
            total_tokens: Some(300),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            ..Usage::default()
        };
        count.fit(&mut later);
        assert_eq!(later.input_tokens, Some(2006));
        assert_eq!(later.output_tokens, Some(300));
        assert_eq!(later.cache_read_tokens, Some(1920));
        assert_eq!(later.cache_write_tokens, Some(12));
        assert_eq!(later.total_tokens, Some(2306));
    }

    #[test]
    fn d280_fit_leaves_a_lone_zero_cache() {
        let mut count = PromptCount::default();
        let mut usage = Usage {
            input_tokens: Some(107),
            output_tokens: Some(2359),
            total_tokens: Some(2466),
            cache_read_tokens: Some(0),
            ..Usage::default()
        };
        count.fit(&mut usage);
        assert_eq!(usage.input_tokens, Some(107));
        assert_eq!(usage.cache_read_tokens, Some(0));
        assert_eq!(usage.total_tokens, Some(2466));
    }

    #[test]
    fn d280_fit_does_not_add_an_omitted_prompt_into_total() {
        let mut count = PromptCount::default();
        let mut first = Usage {
            input_tokens: Some(100),
            total_tokens: Some(101),
            output_tokens: Some(1),
            ..Usage::default()
        };
        count.fit(&mut first);
        let mut later = Usage {
            output_tokens: Some(10),
            total_tokens: Some(110),
            ..Usage::default()
        };
        count.fit(&mut later);
        assert_eq!(later.input_tokens, Some(100));
        assert_eq!(later.total_tokens, Some(110));
    }
}
