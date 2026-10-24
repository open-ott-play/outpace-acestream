# Supplemental dependency license notices

These packages omit standalone license texts from their published crates.
The MIT alternatives below were checked against the upstream revisions recorded
in each vendored `.cargo_vcs_info.json`. The complete release source retains the
original crate contents and its checksum. These supplements do not modify them.

- crab_nat-0.8.1: [upstream Cargo.toml](https://github.com/ryco117/crab_nat/blob/296cc168bb0805fe64b9378d0cc76a833e903ce0/Cargo.toml).
- jni-0.22.4: [upstream LICENSE-MIT](https://github.com/jni-rs/jni-rs/blob/5ae9458a4ec44c5318f37ddc7569c1d4ae8a69e7/LICENSE-MIT).
- jni-sys-macros-0.4.1: [upstream LICENSE-MIT](https://github.com/jni-rs/jni-sys/blob/64d77b7a5f119d7b55b4e2c169a4668067ff59e6/LICENSE-MIT).
- rustls-platform-verifier-android-0.2.0: [upstream LICENSE-MIT](https://github.com/rustls/rustls-platform-verifier/blob/252e25161a91af476cbea620e29d277592d27ec7/LICENSE-MIT).

The jni MIT notice also applies to jni-macros from that same upstream revision.
Crab NAT only declares MIT in Cargo metadata and has no separate notice in its
upstream tree. Its supplement preserves that fact and the declared author,
then supplies the standard MIT terms without inventing a copyright year.
`r-efi` contains its license text and copyright statements in `AUTHORS`, which
the collector includes directly. Future missing license texts fail the release
until a new version-specific review is performed.
