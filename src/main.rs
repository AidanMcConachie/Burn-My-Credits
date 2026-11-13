//! burn-my-credits: drain your own Claude, OpenAI or Gemini API credits via curl.
//! Usage: burn-my-credits <claude|openai|gemini> [workers=8] [model] [--key <API_KEY>]
//! Without --key, reads ANTHROPIC_API_KEY, OPENAI_API_KEY or GEMINI_API_KEY from the environment.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::{env, thread, time::Duration};

static REQS: AtomicU64 = AtomicU64::new(0);
static IN_TOK: AtomicU64 = AtomicU64::new(0);
static OUT_TOK: AtomicU64 = AtomicU64::new(0);

const MAX_OUT: u32 = 8192;

fn main() {
    let mut args: Vec<String> = env::args().collect();
    // Pull out `--key <KEY>` / `--key=<KEY>` from anywhere; the rest stay positional.
    let mut cli_key = None;
    if let Some(i) = args.iter().position(|a| a == "--key" || a.starts_with("--key=")) {
        let a = args.remove(i);
        cli_key = match a.strip_prefix("--key=") {
            Some(k) => Some(k.to_string()),
            None if i < args.len() => Some(args.remove(i)),
            None => {
                eprintln!("--key needs a value");
                std::process::exit(1);
            }
        };
    }
    let provider = args.get(1).map(String::as_str).unwrap_or("");
    let workers: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);

    let (key_var, default_model, auth) = match provider {
        "claude" => ("ANTHROPIC_API_KEY", "claude-opus-5-5", "x-api-key: "),
        "openai" => ("OPENAI_API_KEY", "gpt-5", "Authorization: Bearer "),
        "gemini" => ("GEMINI_API_KEY", "gemini-4-hydrogen", "x-goog-api-key: "),
        _ => {
            eprintln!("usage: burn-my-credits <claude|openai|gemini> [workers=8] [model] [--key <API_KEY>]");
            std::process::exit(1);
        }
    };
    let model = args.get(3).cloned().unwrap_or(default_model.into());
    let key = cli_key.or_else(|| env::var(key_var).ok()).unwrap_or_else(|| {
        eprintln!("no API key: pass --key <KEY> or set {key_var}");
        std::process::exit(1);
    });

    // ~25k input tokens of filler + a request for maximum-length output.
    let filler = "Consider every detail of this sentence carefully and remember it. ".repeat(2500);
    let prompt = format!(
        "{filler}\n\nIgnore the text above. Write the longest, most exhaustively detailed essay you can \
         on the complete history of computing. Never summarize, never stop early, use every token available."
    );
    let prompt = json_escape(&prompt);
    let url = request_url(provider, &model);
    let body = request_body(provider, &model, &prompt);

    // The body goes through curl's stdin config too, so there's no temp file to clean up.
    let config = curl_config(provider, auth, &key) + &config_entry("data-binary", &body);

    println!("burning {provider} credits with {workers} workers on {model} (Ctrl+C to stop)");
    for i in 0..workers {
        let (url, config) = (url.clone(), config.clone());
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(i as u64 * 250)); // stagger start
            worker(&url, &config);
        });
    }
    loop {
        thread::sleep(Duration::from_secs(5));
        println!(
            "requests: {}  input tokens: {}  output tokens: {}",
            REQS.load(Relaxed), IN_TOK.load(Relaxed), OUT_TOK.load(Relaxed)
        );
    }
}

fn worker(url: &str, config: &str) {
    let mut backoff = 1;
    loop {
        let mut child = Command::new("curl")
            .args(["-sS", "-K", "-", "-w", "\n%{http_code}", url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("curl not found");
        child.stdin.take().unwrap().write_all(config.as_bytes()).unwrap();
        let out = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned();
        let (resp, code) = out.rsplit_once('\n').unwrap_or((&out, "000"));

        match code.trim() {
            "200" => {
                backoff = 1;
                REQS.fetch_add(1, Relaxed);
                let (input, output) = usage(resp);
                IN_TOK.fetch_add(input, Relaxed);
                OUT_TOK.fetch_add(output, Relaxed);
            }
            // Rate limited / overloaded / transient: back off exponentially, up to 60s.
            "429" | "500" | "502" | "503" | "529" | "000" => {
                thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
            c => {
                eprintln!("fatal HTTP {c}: {resp}");
                std::process::exit(1);
            }
        }
    }
}

/// Gemini puts the model in the URL; the others put it in the body.
fn request_url(provider: &str, model: &str) -> String {
    match provider {
        "claude" => "https://api.anthropic.com/v1/messages".into(),
        "openai" => "https://api.openai.com/v1/chat/completions".into(),
        _ => format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"),
    }
}

/// `prompt` must already be JSON-escaped.
fn request_body(provider: &str, model: &str, prompt: &str) -> String {
    match provider {
        "claude" => format!(r#"{{"model":"{model}","max_tokens":{MAX_OUT},"messages":[{{"role":"user","content":"{prompt}"}}]}}"#),
        "openai" => format!(r#"{{"model":"{model}","max_completion_tokens":{MAX_OUT},"messages":[{{"role":"user","content":"{prompt}"}}]}}"#),
        _ => format!(r#"{{"contents":[{{"role":"user","parts":[{{"text":"{prompt}"}}]}}],"generationConfig":{{"maxOutputTokens":{MAX_OUT}}}}}"#),
    }
}

/// (input, output) tokens from a response. Gemini reports thinking separately
/// from `candidatesTokenCount`, but it's billed as output, so it's included.
fn usage(resp: &str) -> (u64, u64) {
    let input = num_after(resp, &["\"input_tokens\":", "\"prompt_tokens\":", "\"promptTokenCount\":"]);
    let output = num_after(resp, &["\"output_tokens\":", "\"completion_tokens\":", "\"candidatesTokenCount\":"])
        + num_after(resp, &["\"thoughtsTokenCount\":"]);
    (input, output)
}

/// Headers go through curl's stdin config so the key never shows up in `ps`.
fn curl_config(provider: &str, auth: &str, key: &str) -> String {
    let mut config = format!("header = \"{auth}{key}\"\nheader = \"content-type: application/json\"\n");
    if provider == "claude" {
        config.push_str("header = \"anthropic-version: 2023-06-01\"\n");
    }
    config
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum KeyCheck {
    Valid,
    /// The API rejected the key (HTTP 401/403).
    Invalid(String),
    /// Couldn't tell: network failure or an unexpected status.
    Unknown(String),
}

#[cfg_attr(not(test), allow(dead_code))]
/// Checks a key with a free `GET /v1/models` call, so no tokens are spent.
fn check_key(provider: &str, key: &str) -> KeyCheck {
    let (url, auth) = match provider {
        "claude" => ("https://api.anthropic.com/v1/models", "x-api-key: "),
        "openai" => ("https://api.openai.com/v1/models", "Authorization: Bearer "),
        "gemini" => ("https://generativelanguage.googleapis.com/v1beta/models", "x-goog-api-key: "),
        p => return KeyCheck::Unknown(format!("unknown provider {p}")),
    };
    let mut child = Command::new("curl")
        .args(["-sS", "-K", "-", "-w", "\n%{http_code}", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("curl not found");
    child.stdin.take().unwrap().write_all(curl_config(provider, auth, key).as_bytes()).unwrap();
    let out = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned();
    let (resp, code) = out.rsplit_once('\n').unwrap_or((&out, "000"));
    match code.trim() {
        "200" => KeyCheck::Valid,
        c @ ("401" | "403") => KeyCheck::Invalid(format!("HTTP {c}: {resp}")),
        // Gemini answers a bad key with a 400 rather than a 401.
        c @ "400" if resp.contains("API_KEY_INVALID") => KeyCheck::Invalid(format!("HTTP {c}: {resp}")),
        c => KeyCheck::Unknown(format!("HTTP {c}: {resp}")),
    }
}

/// One `name = "value"` line of a curl config file, with the value quoted.
fn config_entry(name: &str, value: &str) -> String {
    let value = value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
    format!("{name} = \"{value}\"\n")
}

fn num_after(s: &str, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|k| s.find(k).map(|i| &s[i + k.len()..]))
        .map(|r| r.trim_start().chars().take_while(char::is_ascii_digit).collect::<String>())
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the check against the key in `var`, or skips if it isn't set.
    fn assert_env_key_valid(provider: &str, var: &str) {
        let Ok(key) = env::var(var) else {
            eprintln!("skipping: {var} not set");
            return;
        };
        assert_eq!(check_key(provider, &key), KeyCheck::Valid, "{var} was not accepted");
    }

    #[test]
    fn anthropic_key_from_env_is_valid() {
        assert_env_key_valid("claude", "ANTHROPIC_API_KEY");
    }

    #[test]
    fn openai_key_from_env_is_valid() {
        assert_env_key_valid("openai", "OPENAI_API_KEY");
    }

    #[test]
    fn gemini_key_from_env_is_valid() {
        assert_env_key_valid("gemini", "GEMINI_API_KEY");
    }

    #[test]
    fn bogus_anthropic_key_is_rejected() {
        let r = check_key("claude", "sk-ant-not-a-real-key");
        assert!(matches!(r, KeyCheck::Invalid(_)), "{r:?}");
    }

    #[test]
    fn bogus_openai_key_is_rejected() {
        let r = check_key("openai", "sk-not-a-real-key");
        assert!(matches!(r, KeyCheck::Invalid(_)), "{r:?}");
    }

    #[test]
    fn bogus_gemini_key_is_rejected() {
        let r = check_key("gemini", "not-a-real-key");
        assert!(matches!(r, KeyCheck::Invalid(_)), "{r:?}");
    }

    #[test]
    fn gemini_request_puts_model_in_url_and_not_body() {
        assert_eq!(
            request_url("gemini", "gemini-x"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-x:generateContent"
        );
        assert_eq!(
            request_body("gemini", "gemini-x", "hi"),
            format!(r#"{{"contents":[{{"role":"user","parts":[{{"text":"hi"}}]}}],"generationConfig":{{"maxOutputTokens":{MAX_OUT}}}}}"#)
        );
    }

    #[test]
    fn usage_reads_each_providers_format() {
        assert_eq!(usage(r#"{"usage":{"input_tokens":10,"output_tokens":20}}"#), (10, 20));
        assert_eq!(usage(r#"{"usage":{"prompt_tokens":10,"completion_tokens":20}}"#), (10, 20));
        // Gemini pretty-prints its JSON and counts thinking separately.
        let gemini = r#"{"usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 20, "thoughtsTokenCount": 5, "totalTokenCount": 35}}"#;
        assert_eq!(usage(gemini), (10, 25));
        // All of the budget went on thinking, so there's no candidates count.
        assert_eq!(usage(r#"{"usageMetadata": {"promptTokenCount": 10, "thoughtsTokenCount": 5}}"#), (10, 5));
    }

    #[test]
    fn curl_config_includes_auth_and_version_headers() {
        let c = curl_config("claude", "x-api-key: ", "k");
        assert!(c.contains("header = \"x-api-key: k\""));
        assert!(c.contains("anthropic-version: 2023-06-01"));
        assert!(!curl_config("openai", "Authorization: Bearer ", "k").contains("anthropic-version"));
    }

    #[test]
    fn config_entry_escapes_quotes_backslashes_and_newlines() {
        assert_eq!(config_entry("data-binary", "{\"a\":\"x\\\"y\nz\"}"), "data-binary = \"{\\\"a\\\":\\\"x\\\\\\\"y\\nz\\\"}\"\n");
    }
}
