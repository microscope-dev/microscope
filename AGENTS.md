# Agent Instructions

@README.md

We practice Kaizen in our development processes and are always on the lookout to improve. When you find a potential way to improve how we work, flag it to the user for review. Our goal is to maximize total system quality as well as efficiency in our work process.

## Engineering Practices

Always work in the repository’s Nix development environment. Enter it with `./scripts/dev.sh` from the repo root. All tools are pinned via the nix flake.

Use the `justfile` for project tasks. Run `just --list` to see available recipes.

- Keep our systems and code as simple and straightforward as possible, avoiding fancy tricks unless we have specific reason (performance, security, etc.).
- Always ask yourself "what is the best possible architecture for the system and code". We are very open to refactoring our architecture to find global quality maximums.
    - When you find a bug, ask yourself "what is the best possible fix, even if it's a large change" rather than simply putting a local band-aid in place. Present options to the user to choose from if unsure.
- Use all common rust best practices to maintain a high code quality (handle errors rigorously, newtype, make invalid states unrepresentable, etc.).
- Prefer state machines instead of long, complicated logic chains when managing complex logic flows.
- Never add unsafe code without checking with the user. Keep unsafe blocks as small as possible and always test them with miri.
- Never add dependencies without checking with the user.
- When exploring the code, use `rg` to scope down your searches to save tokens.
- We practice poka-yoke wherever possible to ensure our systems are mistake proof.

## Testing

- Always ensure our feedback loops are as fast as possible.
- Use a variety of testing techniques as appropriate for the task at hand and test in-depth (integration tests, e2e tests, user acceptance tests, fuzz tests, property tests, swarm tests, miri, loom, benchmarks, etc.).
- Use unit tests sparingly
- Use `just check` and scope test runs down for fast linter/test runs as you iterate. Then when you reach critical points and want to ensure no regressions, run `just all`, which takes a long time.

## Style Guide

- Never write code comments. Instead, ensure the code speaks for itself because it's so straightforward and simple.
    - Never leave load-bearing assumptions to comments or documentation. Systems and code must be correct by construction.
    - You may write code comments if the compiler/linter requires it (i.e. unsafe).
- Keep files under 1500 lines (excluding tests) and ensure the code is logically organized so it's easy to consume later.

