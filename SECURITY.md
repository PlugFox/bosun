# Security policy

bosun runs coding agents with the user's credentials, so a vulnerability here can expose
tokens, repositories or accounts. Please report it privately.

## Reporting

Use GitHub's private vulnerability reporting: the **Report a vulnerability** button under this
repository's **Security** tab. Do not open a public issue, and do not include real tokens,
transcripts or private repository content in the report; a synthetic reproduction is enough.

You will get an acknowledgement within a week. Fixes land on `master`; there are no supported
release branches while the project is in design and early stages.

## Scope

The threat model is issue #2: identity isolation, prompt injection through the board, verdict
tampering, the local API, secrets in transcripts and agent permissions.
