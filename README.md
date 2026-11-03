# burn-my-credits

Spends your own Claude or OpenAI API credits as fast as it can. It runs several workers in parallel. Each one sends large prompts that ask for the longest possible response, over and over, until you stop it.

> **Warning:** This tool exists to spend money. Use it only with an API key you own, and set spending limits on your account first.

## Requirements

- Rust (edition 2024)
- `curl` on your `PATH`

## Usage

```sh
cargo run --release -- <claude|openai> [workers=8] [model] [--key <API_KEY>]
```

If you don't pass `--key`, it reads `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` from the environment.

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
```

The default models are `claude-opus-5-5` and `gpt-5`. Every 5 seconds it prints how many requests and tokens it has used so far. Press Ctrl+C to stop.

## Notes

- If the API returns a rate-limit or server error, the worker waits and retries, doubling the wait each time up to 60 seconds. Any other error stops the program.
- The API key is passed to curl through stdin, so it doesn't appear in `ps` output.
