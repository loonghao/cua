# DCC compatibility on Driver 0.38.0

This candidate integrates the immutable official `cua-driver-rs-v0.38.0`
release, `85e2b1318bd77ccf3cfa88ccdf875a4bb7eaed19`, with the previously
published DCC compatibility source `be4bbd3eac57796804f43b291a2012dcd2d96896`.
Both histories are retained. The upstream root `LICENSE.md` and Driver package
metadata identify MIT licensing. This is a source compatibility candidate;
it does not install or replace a running Driver or DCC-CUA provider.

## Conflict decisions

- Retain strict same-frame ancestor scopes, scope anchors and scoped
  continuations alongside upstream's bounded tab-visibility fallback. Missing
  or conflicting scope identity still refuses rather than broadening the read.
- Preserve explicit hidden-file-input associations and foreground navigation
  readback while adopting upstream's select and DOM-click effect tests.
- The mock CDP server distinguishes closing before a reply from withholding a
  reply on a live connection. Both behaviors remain independently testable.
- DOM traversal retains the fork's frame and shadow-tree context, and inherits
  upstream's transparent-ancestor classification using that same context.
- Use upstream's shared, ancestry-only process selection. Keep the additional
  pre-launch PID baseline filter for owned launch candidates; executable names
  never prove ownership. No process enumeration is needed by the pure tests.
- Keep complete native UIA read failure distinct from a proven leaf, alongside
  upstream's node/depth/time completeness budget and its synthetic tests.
- Transfer the compatibility parameter descriptions to upstream's reorganized
  `pointer-cursor-and-browser.mdx`; do not restore deleted tool-reference pages.
- Preserve bounded helper console suppression, owned setup-tab cleanup,
  responsive shared-socket release and opt-in download source metadata.

## Validation and limits

Run exact browser mock and semantic/download tests, pure Windows launch-selection
and synthetic UIA tests, then downstream compilation and dependency-contract
checks against one immutable compatibility revision. Test selection must be
reviewed before execution: native helper, consent, cursor, clipboard and actual
application tests require separate authorization and are not part of this run.

Official 0.38.0 includes launch-family, foreground scroll scaling and guarded
foreground drag fixes. Those changes do not prove that a reported console flash,
UIA timeout, or out-of-desktop capture refusal is fixed. In particular, the
presence of a Host and a client-owned MCP bridge is not evidence of a process
leak. Runtime acceptance requires the original owner's exact process and window
binding; source tests cannot establish OS process reaping, browser native-host
acceptance, or an absence of visible console flashes.
