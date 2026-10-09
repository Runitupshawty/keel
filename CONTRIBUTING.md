# Contributing to Keel

Thank you for helping improve Keel.

1. Fork [the repository](https://github.com/Runitupshawty/keel) and create a focused branch from `main`.
2. Make one logical change per pull request.
3. Add or update tests for behavior changes. New filesystem, search, preview, remote, or cloud providers must include provider tests.
4. Before opening the pull request, run:

   ```powershell
   cargo fmt --all --check
   cargo clippy --all-targets -- -D warnings
   cargo test --workspace
   ```

5. Explain the problem, the solution, and how you verified it in the pull request description.

Do not commit machine-specific configuration, hostnames, IP addresses, usernames, credentials, access tokens, or OAuth client secrets. Personal profiles belong under `%APPDATA%\Keel`, outside the repository.
