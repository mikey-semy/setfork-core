# Contributing to setfork-core

`setfork-core` is the Rust git service behind SetFork. It serves heavy git operations
and the domain read/write ports over gRPC. Contracts live in `proto/`.

Before anything else: **this project is maintained by one person.** Pull requests are
reviewed roughly once a week. That is not a promise of speed — it is a promise of an
answer. Silence is worse than a stated wait, so if a week passes without a word, ping
the thread.

## Before you write code

Open an issue first for anything beyond an obvious fix. A rejected design costs you an
afternoon; a rejected pull request costs you a week and the maintainer a review.

Small and self-contained is easier to accept than large and sweeping. If your change
touches the proto contracts, say so in the issue — those files are mirrored in the
application repository and are checked byte-for-byte by a gate, so a contract change is
always a two-repository change.

## AI Contribution Policy

Contributions made with the assistance of AI tools are welcome, but contributors must
use them responsibly and disclose that use clearly.

1. Review AI-generated code closely before marking a pull request ready for review.
2. Manually test the changes and add appropriate automated tests where feasible.
3. Only use AI to assist in contributions that you understand well enough to explain,
   defend, and revise yourself during review.
4. Disclose AI-assisted content clearly.
5. Do not use AI to reply to questions about your issue or pull request. **The questions
   are for you, not an AI model.**
6. AI may be used to help draft issues and pull requests, but contributors remain
   responsible for the accuracy, completeness, and intent of what they submit.

Maintainers reserve the right to close pull requests and issues that do not disclose AI
assistance, that appear to be low-quality AI-generated content, or where the contributor
cannot explain or defend the proposed changes themselves.

*This policy follows [Gitea's](https://github.com/go-gitea/gitea/blob/main/CONTRIBUTING.md),
and we keep it for a concrete reason. In January 2026 the curl project shut down its bug
bounty programme because AI-generated reports consumed more maintainer time than they
were worth. One maintainer cannot absorb that. SetFork is a product whose audience
includes AI agents, and our own MCP tools file issues — so the rule that a human stands
behind every submission matters here more than in most projects, not less.*

## Testing

```sh
cargo test                       # unit tests
cargo test -- --include-ignored  # plus tests that need Postgres
```

⚠️ Tests that need a database are marked `#[ignore]`, so a plain `cargo test` **passes
while silently skipping them**. CI runs them with `--include-ignored`; run it that way
too before you push, or CI will tell you what your green run did not.

Bring a database up with `bash scripts/itest-env.sh` and export the printed
`TEST_DATABASE_URL`.

Probes live behind a feature flag and are not built by default: `--features probes`.

**A test that cannot fail guards nothing.** When you add a guard, break the rule it
guards and watch it go red. A green test proves the test ran, not that it works.

## Style

Run `cargo fmt` and `cargo clippy` before pushing; CI checks both.

Identifiers are Latin-only. Comments explain *why*, not *what* — the code already says
what it does. Comments in this repository are in Russian; keep writing them in the
language of the file you are editing.

## Developer Certificate of Origin (DCO)

We consider the act of contributing to the code by submitting a Pull Request as the
"Sign off" or agreement to the certifications and terms of the
[DCO](https://developercertificate.org). Adding the `Signed-off-by` line with
`git commit -s` is appreciated but optional.

⚠️ **A Contributor License Agreement will be required before the first external pull
request is merged into this repository.** The core is licensed under AGPL-3.0, and the
project sells exceptions to it; without a CLA that right is lost quietly and cannot be
recovered afterwards. The agreement does not exist yet — if you are about to open your
first pull request here, say so in the issue and we will sort it out before you spend
the effort, not after.

## License

By contributing you agree that your work is licensed under
[AGPL-3.0-only](LICENSE), the licence of this repository.

⚠️ AGPL section 13 applies to network use: anyone interacting with a modified version
over a network must be able to obtain its source. If you run a modified SetFork as a
service, that obligation is yours.

## Security

Do not report vulnerabilities as public issues. See [SECURITY.md](SECURITY.md).
