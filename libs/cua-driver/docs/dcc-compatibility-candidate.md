# DCC compatibility source candidate

The candidate is based on official stable Driver 0.33.2, tag
cua-driver-rs-v0.33.2 at c82d32e3e1adbc6578a148962002ddf6e3e8a15a.
Its actual pull-request baseline is the CI-only staging commit
8ba30be06ba33c8c6f1ebb2439e6b9a64c46ad7f. That baseline leaves the official Rust tree
unchanged and guards automatic fork publication events before branch creation.
It replays necessary author-preserving browser compatibility changes from the
DCC fork. The prior reviewed source checkpoint is
7aef51f2eb8f29a57c94c3f4b11ee4b61b085965.

The retained public contracts are ancestor role scope and scope anchors,
browser_scope_unavailable, unique hidden file-input associations, native
consent ownership and ambiguity refusal, and foreground navigation receipts.
Replacing this revision with unmodified upstream would remove those contracts.
Public Rust enum variants and struct fields also changed; source callers using
exhaustive matches or struct literals must evaluate compatibility.

The source CI selects and asserts a full candidate SHA. It checks formatting,
strict whole-package lint and all test-target compilation. It does not execute
the whole default Windows suites: some tests invoke real token/process/ACL
helpers. Historical memory and MockCDP regression evidence is recorded separately
from the source compile results. This SDK source job targets Windows; other
platform compile and canonical native acceptance remain separate gates.

For this fork's candidate base and candidate push branches, native or unproved
workflows are deferred while the pull request is draft. Existing canonical
steps and conditions are retained, and ready_for_review is an explicit event.
These skipped jobs are pending evidence, not native acceptance. The canonical
desktop matrix must run on the final candidate before ready or merge. No
release tag, distribution promotion or installed runtime change is made by this
source draft.
