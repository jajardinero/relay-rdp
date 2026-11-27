# Relay patch to russh 0.65.4

Relay's NLA sign-in stack (sspi, ironrdp-connector, picky, winscard) pins the
RustCrypto release candidates `rsa =0.10.0-rc.18` and `pkcs1 =0.8.0-rc.4`; every
release of those crates up to picky 7.0.0-rc.29 and sspi 0.23.1 still does.
russh 0.65.2 moved to the final `rsa 0.10` and `pkcs1 0.8` (with `ssh-key
0.7.0-rc.12`), and Cargo can't build a release candidate and its final release
into one binary.

This copy of russh 0.65.4 goes back to the dependency set of russh 0.65.1, the
last release on those release candidates, while keeping the fixes of 0.65.3 and
0.65.4 (seven security advisories, several of them in the client):

- `Cargo.toml`: `rsa = "=0.10.0-rc.18"`, `pkcs1 = "=0.8.0-rc.4"`,
  `ssh-key = "=0.7.0-rc.11"`.
- `src/keys/format/pkcs8.rs`, `src/keys/format/mod.rs`: the release candidates'
  names for two types, `rsa::RsaPrivateKey` (`rsa::PrivateKey` in 0.10) and
  `pkcs1::RsaPrivateKey` (`pkcs1::RsaPrivateKeyRef` in 0.8).

Remove this patch when picky, sspi and winscard accept `rsa 0.10`.
