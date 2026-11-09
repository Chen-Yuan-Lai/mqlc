//! USD prices per million tokens, for the running cost shown by `mqlc bench`.

use crate::llm::Usage;

struct Price {
    input: f64,
    output: f64,
}

// Anthropic list prices; cache writes (5-minute TTL) cost 1.25× input and
// cache reads 0.1× input.
fn price(model: &str) -> Option<Price> {
    let (input, output) = match model {
        m if m.starts_with("claude-haiku-4-5") => (1.0, 5.0),
        m if m.starts_with("claude-sonnet-4-5") => (3.0, 15.0),
        m if m.starts_with("claude-opus-4-5") => (5.0, 25.0),
        _ => return None,
    };
    Some(Price { input, output })
}

/// Cost of one call, or `None` for models without a known price.
pub fn cost_usd(model: &str, usage: &Usage) -> Option<f64> {
    let p = price(model)?;
    let tokens = |n: u64, per_million: f64| n as f64 * per_million / 1e6;
    Some(
        tokens(usage.input_tokens, p.input)
            + tokens(usage.cache_creation_input_tokens, p.input * 1.25)
            + tokens(usage.cache_read_input_tokens, p.input * 0.1)
            + tokens(usage.output_tokens, p.output),
    )
}
