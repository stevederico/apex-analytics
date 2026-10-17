//! Model picker lists served from `/api/version`. Presentation only here —
//! dottie-pro enforces the Pro allowlist.

/// Models a Pro client may ask for, as `id:Display Name` pairs.
/// Override with `PRO_MODELS`. First entry is the client default.
const DEFAULT_PRO_MODELS: &str =
    "grok-4.6:Grok 4.6,grok-4.5:Grok 4.5,grok-4.3:Grok 4.3,\
     grok-4.20-0309-reasoning:Grok 4.20 Reasoning,\
     grok-4.20-0309-non-reasoning:Grok 4.20";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProModel {
    pub id: String,
    pub name: String,
}

pub fn parse_pro_models(raw: &str) -> Vec<ProModel> {
    parse_models_or(raw, DEFAULT_PRO_MODELS)
}

pub fn default_byok_models(provider: &str) -> &'static str {
    match provider {
        "openai" => "gpt-5:GPT-5,gpt-5-mini:GPT-5 Mini,gpt-5-nano:GPT-5 Nano,\
                     gpt-4o:GPT-4o,gpt-4o-mini:GPT-4o Mini,gpt-4.1:GPT-4.1,\
                     gpt-4.1-mini:GPT-4.1 Mini,gpt-4.1-nano:GPT-4.1 Nano,o3-mini:o3-mini",
        "anthropic" => "claude-opus-5:Claude Opus 5,claude-sonnet-5:Claude Sonnet 5,\
                        claude-fable-5:Claude Fable 5,claude-haiku-4-5-20251001:Claude Haiku 4.5",
        "cerebras" => "llama3.1-8b:Llama 3.1 8B,qwen-3-235b-a22b-instruct-2507:Qwen 3 235B,\
                       gpt-oss-120b:GPT-OSS 120B,zai-glm-4.7:ZAI GLM 4.7",
        _ => "",
    }
}

pub fn parse_models_or(raw: &str, fallback: &'static str) -> Vec<ProModel> {
    let parsed: Vec<ProModel> = raw
        .split(',')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, ':');
            let id = it.next().unwrap_or("").trim();
            if id.is_empty() {
                return None;
            }
            let name = it.next().unwrap_or("").trim();
            Some(ProModel {
                id: id.to_string(),
                name: if name.is_empty() { id.to_string() } else { name.to_string() },
            })
        })
        .collect();
    if parsed.is_empty() && !fallback.is_empty() {
        parse_models_or(fallback, "")
    } else {
        parsed
    }
}
