//! burn-my-credits: drain your own Claude or OpenAI API credits via curl.
//! Usage: burn-my-credits <claude|openai> [workers=8] [model] [--key <API_KEY>]
//! Without --key, reads ANTHROPIC_API_KEY or OPENAI_API_KEY from the environment.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::{env, fs, thread, time::Duration};

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

    let (url, key_var, default_model, auth) = match provider {
        "claude" => ("https://api.anthropic.com/v1/messages", "ANTHROPIC_API_KEY", "claude-opus-5-5", "x-api-key: "),
        "openai" => ("https://api.openai.com/v1/chat/completions", "OPENAI_API_KEY", "gpt-5", "Authorization: Bearer "),
        _ => {
            eprintln!("usage: burn-my-credits <claude|openai> [workers=8] [model] [--key <API_KEY>]");
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
    let body = if provider == "claude" {
        format!(r#"{{"model":"{model}","max_tokens":{MAX_OUT},"messages":[{{"role":"user","content":"{prompt}"}}]}}"#)
    } else {
        format!(r#"{{"model":"{model}","max_completion_tokens":{MAX_OUT},"messages":[{{"role":"user","content":"{prompt}"}}]}}"#)
    };

    let body_path = env::temp_dir().join(format!("burn-body-{}.json", std::process::id()));
    fs::write(&body_path, &body).expect("write body");
    let config = curl_config(provider, auth, &key);

    println!("burning {provider} credits with {workers} workers on {model} (Ctrl+C to stop)");
    for i in 0..workers {
        let (url, config, body_path) = (url.to_string(), config.clone(), body_path.clone());
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(i as u64 * 250)); // stagger start
            worker(&url, &config, body_path.to_str().unwrap());
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

fn worker(url: &str, config: &str, body_path: &str) {
    let mut backoff = 1;
    loop {
        let mut child = Command::new("curl")
            .args(["-sS", "-K", "-", "-w", "\n%{http_code}", "--data-binary"])
            .arg(format!("@{body_path}"))
            .arg(url)
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
                IN_TOK.fetch_add(num_after(resp, &["\"input_tokens\":", "\"prompt_tokens\":"]), Relaxed);
                OUT_TOK.fetch_add(num_after(resp, &["\"output_tokens\":", "\"completion_tokens\":"]), Relaxed);
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
        c => KeyCheck::Unknown(format!("HTTP {c}: {resp}")),
    }
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
    fn curl_config_includes_auth_and_version_headers() {
        let c = curl_config("claude", "x-api-key: ", "k");
        assert!(c.contains("header = \"x-api-key: k\""));
        assert!(c.contains("anthropic-version: 2023-06-01"));
        assert!(!curl_config("openai", "Authorization: Bearer ", "k").contains("anthropic-version"));
    }
}
