# Contributing

Booth is a small project and I work on it in my spare time, so this is short.

## Bugs

Open an issue with the bug report form. It asks for what happened, your Booth and Windows versions and the log, and it says how to get the log. Edit IP addresses and your user name out of the log if you do not want them public.

## Changes

Small fixes are welcome as pull requests. For anything bigger, open an issue first so we agree on it before you spend time on it.

Before you open a pull request: one change per pull request, a test where it makes sense, `cargo test --workspace --locked`, `cargo clippy --workspace --locked` and `cargo clippy --workspace --all-targets --locked` all clean, and code and comments that read like the code around them. The README says how to build.

Anything that changes what goes over the wire, the invite format or what an older Booth in the same room understands needs an issue first. Those have to stay compatible.

## Security

Do not open a public issue for a security problem. SECURITY.md says how to report one.
