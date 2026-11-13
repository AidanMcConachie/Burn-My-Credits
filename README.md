## Usage

```sh
cargo run --release -- <claude|openai|gemini> [workers=8] [model] [--key <API_KEY>]
```

If you don't pass `--key`, it reads `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GEMINI_API_KEY` from the environment.

The arguments are positional, so to choose a model you also have to give the number of workers.

Examples:

```sh
# Claude with the defaults (8 workers, claude-opus-5-5), key from the environment
export ANTHROPIC_API_KEY=sk-ant-...
cargo run --release -- claude

# OpenAI with 16 workers on the default model (gpt-5), key passed on the command line
cargo run --release -- openai 16 --key sk-...

# Claude with 4 workers on a cheaper model
cargo run --release -- claude 4 claude-haiku-5-5

# Gemini with the defaults (8 workers, gemini-4-argon), key from the environment
export GEMINI_API_KEY=...
cargo run --release -- gemini
```

The default models are `claude-opus-5-5`, `gpt-5` and `gemini-4-argon`. Every 5 seconds it prints how many requests and tokens it has used so far. Press Ctrl+C to stop.

## Notes

- If the API returns a rate-limit or server error, the worker waits and retries, doubling the wait each time up to 60 seconds. Any other error stops the program.
- The API key is passed to curl through stdin, so it doesn't appear in `ps` output.
- `gemini-4-argon` isn't available on Gemini's free tier, so pass a different model if your key is on that tier.
- Gemini's output token count includes thinking tokens.
