# Checkout lock v1 conformance fixtures

These files are byte-for-byte copies of the authoritative fixtures owned by
`cgraf78/actions` at the immutable revision recorded in
[`.github/cgraf78-actions.lock`](../../../.github/cgraf78-actions.lock). Shdeps
consumes the same records, wire values, state transitions, and checkout-root
vectors so the generated installer and the compiled manager cannot silently
develop incompatible lock or path semantics.

The corresponding public protocol is vendored at
[`docs/checkout-lock-v1.md`](../../../docs/checkout-lock-v1.md). Update the
protocol and fixtures together from one reviewed Actions revision; do not
locally reinterpret or normalize their bytes.

The Rust conformance tests pin these SHA-256 values as an intentional review
gate:

```text
079862d29dd149da06c864a5ddce7881efe7521287b8e9c0bb26c6da1f46ceac  checkout-lock-v1-owner-record.txt
6cd66571393026e646bef03460b87ebbbcdd11e6680ae9da6618fc201d85312b  checkout-lock-v1-claim-record.txt
fe5a79468ec4805d3a2b46fc4ea01ebbce360b17e990b4344b5c845b28318ad7  checkout-lock-v1-records.tsv
ed9d71c22ee67933257d94ec14dcf9174ba405a0dba84719bf406372020b82a0  checkout-lock-v1-states.tsv
1a1fe2ebc8dfef7792bcde39f9ee2895f1cc0d1dac94c0cc9dd0f3c2890bdd67  checkout-lock-v1-wire.tsv
dd7cf58ad1f328efcc3e86497857414fff2e86ee596c2eff222feece6e626b76  shdeps-root-v1.tsv
```
