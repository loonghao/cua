# DCC compatibility on Driver 0.36.0

This branch is an integration candidate for dcc-mcp/dcc-cua. Its base is the
unmodified official `cua-driver-rs-v0.36.0` release,
`6d6c734c127025fb39ad5d585a10382808b39dc1`. It is not a separate official
Driver release. Source compatibility, native qualification, downstream release
publication, and installed-client acceptance are separate gates.

## Retained contracts

The previous DCC pin was `3d2bcf50089ea18aa5bedab328b1afb3f710ea00`, based on
Driver 0.28.2. The following existing compatibility changes remain necessary:

- Ancestor-scoped semantic snapshots: `scope_ancestor_role`, `scope_anchor`,
  and `browser_scope_unavailable`. A failed scope proof never broadens to the
  whole page. The current skill documents these fields.
- Hidden file-input associations with unique, explicit label evidence.
- Explicit foreground tab activation and navigation completion receipts,
  including frame/loader binding, live target revalidation, and partial or
  unknown completion results. Readback is fenced by frame/loader/URL proof
  both before and after DOM evaluation. An untyped CDP navigation error cannot
  prove whether dispatch occurred: return `dispatched: null`,
  `dispatch_state: "unknown"`, and `navigation_completion_unknown`, invalidate
  old refs, and require fresh state before any retry. The regression applies
  the navigation at the mock CDP server and drops its reply.
- A responsive `Target.getTargets` probe before publishing an existing-profile
  attachment. Driver 0.36.0 shares consented sockets: a failed probe releases
  only the current grant's hold with `SharedSocketRelease::Close`. It must not
  evict a peer session's socket. Tests cover both single-holder replacement and
  peer-holder retention.
- Windows native consent-window PID/HWND/owner-chain identity, liveness,
  visibility, enabled-state and topmost disambiguation.
- Console-free Windows child-process construction for bounded helpers. Their
  existing stdio, wait and cleanup behavior is retained.

## Superseded changes and conflict decisions

- The old fresh-claim consent patch is already upstream, with newer
  `BrowserConsentAction` cancellation/settlement handling. Keep the official
  implementation rather than replaying it.
- Repeated consent-surface deduplication is upstream and now uses opaque title
  plus allow/cancel geometry, including re-created UIA pointers. Preserve it
  while layering exact native-window ownership. Keep `ConsentNodes` RAII for
  both the original tree and every additional native-window tree.
- The old Windows CI module filter is unnecessary; upstream runs the whole
  platform-windows library test suite.
- The old `netstat.exe` construction hunk is obsolete; upstream uses the native
  TCP listener API. The other helper callsites still need console suppression.
- Old website documentation paths were deleted upstream; do not resurrect
  them. Preserve compatibility guidance in the current Driver skill and this
  integration record.
- Merge the new navigation fixture state with upstream windowless-target and
  target-disappearance fixtures. Do not replace either test family.

## Unmerged-candidate preservation

The installed 0.28.2 pin is not the only migration input. Two existing fork
PRs contain reviewed fixes that official 0.36.0 still lacks:

- [PR #5](https://github.com/loonghao/cua/pull/5), Driver 0.33.2 candidate:
  retain `92a0be300ffd896b1d9bdf44515a89880fc4d18f` mixed duplicate/unique
  `aria-controls` and label-for rejection, with pure semantic and mock-CDP
  regressions; retain `fa80a8e3560641dd9e732a755afd467339cf6ba3` immediate
  `ManuallyDrop` ownership for the borrowed scroll UIA element. Keep native
  console-visibility tests opt-in with explicit canonical runner selections.
- [PR #4](https://github.com/loonghao/cua/pull/4), Windows launch ownership:
  retain the production delta through `b60ac0172e70e2ccecd087fd6df3f7ac749c7284`.
  Late-window resolution and minimized polling use the native launch PID and
  new descendants, never unrelated executable-name matches. Keep the public
  `related_processes` discovery API. The current whole Windows library suite
  already selects all six pure ownership tests; no old CI filter is needed.

These drafts are comparison inputs, not native qualification of this new
candidate. Their branches, PR states, and the separate attribution-policy PR
are unchanged. Prior source-lint moves and CI staging changes are not replayed
when the official 0.36 source already satisfies the current gate.

## Migration regression corrections

- Generic AX traversal uses renderer-target identity, so same-process iframe
  children remain in direct scopes and outlines while OOPIF id collisions stay
  isolated. Ancestor capability validation still requires exact frame/document
  proof. This distinction also appears in [upstream PR #5387](https://github.com/trycua/cua/pull/5387); that work is
  already owned upstream and is not duplicated here.
- Upload associations stop at a shadow/document host boundary without dropping
  an already proven local association. Implicit labels containing competing
  labelable controls refuse upload rather than selecting an unrelated file
  input. Hidden-type inputs are not labelable; CSS-hidden file inputs are.
- Provider-facing browser schemas stay flat for Bedrock/Vertex compatibility.
  Invocation still enforces ancestor scope/ref/query requirements and semantic
  continuation conflicts; removing unsupported advertised `allOf` does not
  remove those runtime checks. Legacy bind-mode handling of non-ancestor
  optional fields is unchanged.
  The live protocol roster includes explicit foreground browser navigation.
- An embedding host that intentionally clears every registered tool retains an
  empty dispatch surface. The new default visual-parser fallback must not
  repopulate a pixels-only runtime after that hook. Canonical and custom
  perception inventories keep their existing behavior.
- Generated browser references are refreshed from the exact native Linux,
  macOS and Windows registry snapshots produced by the compatibility CI. The
  canonical generator renders the shared pages; schemas are not hand-edited.

Three actual semantic owner tests failed before the boundary corrections and
passed afterward, and the existing schema/runtime test failed on `allOf`
before the provider correction. The source-built CLI direct-MCP protocol
schema test passes with the flat schema and navigation roster. Browser tests
are CPU and mock-CDP results, not live browser or native Windows/macOS
acceptance.

## Qualification boundaries

The Windows matcher source can be tested without Win32, but that does not
verify COM ownership, native invocation, cancellation, or actual Edge setup.
Windows and macOS native acceptance must run on their own platforms. A Linux
build or mock CDP fixture is never evidence that those platforms passed.
