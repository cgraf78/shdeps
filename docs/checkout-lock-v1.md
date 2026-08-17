# Checkout mutation lock protocol v1

This protocol serializes writes to one generated checkout between the Actions
checkout installer and other managers such as Shdeps. It is opt-in through
`CHECKOUT_INSTALLER_LOCK_PROTOCOL=v1`; an unset setting keeps the historical
empty-directory lock byte-for-byte unchanged.

The protocol is a same-user correctness boundary, not a sandbox against a
process that can arbitrarily rewrite the caller's home directory. Even so,
every path and record is validated before it can authorize recovery.

## Paths

For a normalized checkout `/parent/name`, the canonical lock is:

```text
/parent/.name.install.lock
```

A live v1 lock is a relative symlink whose literal target is exactly:

```text
.name.install.lock.owner.<owner-nonce>/.
```

The target resolves to a private same-parent directory. The literal `/.`
suffix is intentional portability hardening: if a legacy directory wins the
publication race, GNU/BSD `ln -s TARGET EXISTING_DIR` tries to create the
basename `.` and fails instead of nesting a stray link inside the winner. An
owner nonce is exactly 32 lowercase hexadecimal characters and is never reused
while any owner or claim path for that generation remains.

A process that releases or recovers an owner prepares a private claim directory
whose basename binds both generations:

```text
.name.install.lock.claim.<owner-nonce>.<claimant-nonce>
```

The claim contains its record plus, after the linearization point, the complete
owner directory at `owner/`. The canonical tombstone travels inside that
directory as `owner/canonical`. After detachment, the claimant atomically moves
that directory to the cleanup-only `retired/` slot before removing individual
files. `retired/` never participates in lock arbitration; an interrupted
cleanup can therefore leave inert private debris without blocking a fresh
generation.

## Records

Records are regular, non-symlink files with mode `0600` inside regular,
non-symlink directories with mode `0700`. Parsers read fixed lines and never
source or evaluate them. Every nonnumeric value is lowercase even-length hex;
decoded values are bounded by the implementation before use.
The wire record is exactly nine newline-terminated lines. NUL bytes, a missing
final newline, extra bytes, duplicate fields, and reordered fields are invalid;
raw-byte validation happens before Bash `read` can discard unrepresentable
NULs.

Owner record, stored as `owner-v1`:

```text
cgraf78 checkout mutation lock v1
role=6f776e6572
nonce=<owner nonce hex>
owner_nonce=
pid=<positive decimal>
host_hex=<hex of LC_ALL=C uname -n bytes>
start_kind_hex=<hex of proc-stat or ps-lstart>
start_token_hex=<hex of the opaque process-start token>
checkout_hex=<hex of the normalized absolute checkout path>
```

Claim record, stored as `claim-v1`:

```text
cgraf78 checkout mutation lock v1
role=636c61696d
nonce=<claimant nonce hex>
owner_nonce=<owner nonce hex>
pid=<positive decimal>
host_hex=<hex of LC_ALL=C uname -n bytes>
start_kind_hex=<hex of proc-stat or ps-lstart>
start_token_hex=<hex of the opaque process-start token>
checkout_hex=<hex of the normalized absolute checkout path>
```

Unknown, duplicate, missing, extra, oversized, non-hex, or otherwise malformed
fields fail closed. The directory names, record nonces, owner binding, literal
canonical target, and normalized checkout identity must all agree.

## Liveness

Automatic recovery requires positive evidence that both the recorded owner and
any claimant are dead on the current host.

- Linux and Termux prefer the process start ticks from `/proc/<pid>/stat` and
  treat a zombie state as dead.
- macOS and other supported Unix hosts use the `LC_ALL=C ps -o lstart=` value.
  Before hex encoding, implementations apply locale-C awk field
  normalization (`{$1=$1; print}`): leading/trailing whitespace is removed and
  every internal whitespace run becomes one ASCII space. The shared wire
  fixture includes raw and normalized bytes for this rule.
- A missing PID or a demonstrably different start token is dead.
- A matching PID and start token is live.
- A different host, unsupported start backend, permission failure, malformed
  probe, or otherwise ambiguous result is unknown and therefore treated as
  live until timeout.

Actions waits for `CGRAF78_CHECKOUT_INSTALL_LOCK_TIMEOUT_SECS`; Shdeps waits for
`SHDEPS_CHECKOUT_LOCK_TIMEOUT_SECS`. Both use the same strict nonnegative
decimal-integer grammar, normalize leading zeroes, reject values longer than
nine digits before shell arithmetic, and use a 1800-second default. Zero
performs one immediate classify, recover, and acquire attempt plus a final
read-only winner classification. Implementations emit at most one wait
notice, then a timeout diagnostic naming the canonical lock and trustworthy
owner information. A legacy empty-directory lock is never removed
automatically and retains the exact `rmdir` recovery guidance.

## Acquisition

1. Reject a non-directory parent or malformed canonical object.
2. If the canonical path is absent, prepare the complete private owner
   directory and record, rename it to its final unique basename, and atomically
   publish the relative canonical symlink.
3. Re-read the canonical symlink and owner record. Acquisition succeeds only
   when every identity matches this process and checkout.
4. If publication lost a race, remove only the caller's known record and empty
   owner directory, then classify the winner.
5. After acquisition, recover any interrupted checkout publication transaction
   before starting a new checkout mutation.

Legacy installers use `mkdir` at the same canonical path, so a v1 symlink
blocks them and a legacy directory blocks v1.

## Release and stale recovery

Release and recovery share one ownership-transfer state machine:

1. Prepare a unique claim directory and complete claim record.
2. Revalidate the exact canonical generation and positive authority to act.
3. Atomically rename the owner directory to the guaranteed-absent
   `claim/owner`. This same-filesystem rename is the linearization point. Only
   the process holding the nested owner directory may detach the canonical
   symlink.
4. Revalidate the nested owner record. Before detachment it must contain no
   tombstone and the canonical path must still be the exact original relative
   symlink.
5. Rename the canonical symlink to `claim/owner/canonical`, verify its literal
   target, and consider detachment committed. From this point onward the
   claimant never reads, removes, or renames the canonical path again.
6. Atomically rename `claim/owner` to the guaranteed-absent `claim/retired`.
   This is the cleanup commit point: scanners ignore claims without an active
   `owner` slot, so leaf removal cannot expose a half-valid arbitration state.
7. Remove only the validated retired tombstone, fixed record files, and empty
   private directories with leaf `rm` plus `rmdir`. Never recursively delete a
   path derived from a link or record.

If a claimant dies before detachment, the canonical symlink is dangling and
exactly one validated claim contains the owner directory. A new claimant may
take over only after positive proof that the prior claimant is dead, and only
by atomically moving that complete nested owner directory into its own private
claim. If the nested owner already contains the validated tombstone, or the
canonical path is absent or belongs to a different generation, detachment has
already committed: recovery may clean only the old claimed paths and must not
touch the canonical path. A claim with no `owner` slot is either still being
prepared or has entered retired cleanup and is outside arbitration.

Zero or multiple owner-bearing claims for a dangling canonical path, a live or
unknown claimant, malformed links or records, unexpected files, and nonempty
legacy directories all fail closed. Zero claims are treated as a possible
handoff snapshot only until the bounded timeout; multiple claims and stable
malformation fail immediately. Empty or pre-publication orphan claim
directories are harmless and need no broad garbage collector.

## Required conformance coverage

Both implementations consume the exact fixture bytes under
`checkout-installer/fixtures/checkout-lock-v1-*`. The two record files are
parsed verbatim, while the TSVs define literal canonical targets, process-token
normalization, and symbolic state transitions. They cover:

- default legacy-byte preservation and strict opt-in validation;
- live waiting, timeout zero, wait-to-success, and quiet diagnostics;
- legacy directory interoperability;
- PID reuse, zombies, remote hosts, and unknown probes;
- malformed records and hostile symlink targets;
- SIGKILL before and after owner publication;
- SIGKILL before claim, after the owner claim, and after canonical detachment;
- a fresh acquisition while a prior detached claim is in interruptible leaf
  cleanup;
- two reclaimers racing with a new v1 writer and a legacy `mkdir` writer;
- transaction recovery only after a fresh canonical acquisition; and
- ownership-checked normal release and unlocked post-install delegation.
