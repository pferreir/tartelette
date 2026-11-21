# Contributing

Thanks for your interest in tartelette. Bug reports, fixes, new devices and
boards are all welcome.

## Bug reports

The most useful report is a minimal firmware (ELF or UF2) and board file that
show the problem, with what you expected and what happened instead. Say
which build you ran (native or browser, version or commit).

## Pull requests

- **Small and focused.** One fix, one device, one feature per pull request.
  Unrelated cleanups, reformatting and refactors go in pull requests of
  their own. Large changes are much easier to land if you open an issue to
  discuss the design first.
- **One-line commits** with an area prefix, e.g.
  `[core] Raise the DMA IRQ on chain completion`. The areas are listed in
  [AGENTS.md](AGENTS.md#commits).
- **Checks pass:** `cargo fmt`, `cargo clippy`, `cargo test --workspace` and
  the browser build (see [AGENTS.md](AGENTS.md#before-committing) and
  [docs/development.md](docs/development.md)). If something can't run on
  your machine, say so in the pull request.
- **Tests** for new behaviour, ideally a firmware run headless in
  `crates/tartelette/tests`.
- **Documentation** updated in the same pull request: `docs/`, the
  `README.md` device list, the board schema.
- **Determinism and the web build** are not negotiable: the core crates stay
  `no_std`, do no I/O, and two runs with the same inputs stay identical.

## Use of LLMs

Much of tartelette was written with LLMs, and you're welcome to use them
for your contributions too. You are responsible for every line you submit:
understand it, test it, and be ready to explain it.

**The conversation, however, has to be human.** Pull request and issue
descriptions, comments and replies to review must be written by you, in
your own words. Pasting text straight out of an LLM, or having an "AI
participant" open pull requests, file issues or answer reviews, won't be
tolerated: such contributions will be closed without review.

Short and imperfect is fine. We'd rather read two sentences you wrote than
two pages you didn't.

## License

By contributing, you agree that your contributions are licensed under the
GNU General Public License v3, as the rest of the project (see
[LICENSE](LICENSE)).
