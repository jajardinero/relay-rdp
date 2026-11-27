# Relay patches to sspi 0.21.3

The CredSSP client always sends TSRequest version 6. From its pubKeyAuth
message on, it also sent the `clientNonce` field ([5]) whatever version the
server spoke. Servers older than CredSSP 5 don't define that field. Windows
Server 2008 (CredSSP 2) resets the connection when it arrives, so NLA sign-in
to it failed with "Connection reset by peer". The client now sends the nonce
only to peers of version 5 or later, as FreeRDP does. The public key proof
already used the version-appropriate method (echo below 5, hash from 5).

Remove this patch when an sspi release does the same.

## picky 7.0.0-rc.26

`Cargo.toml` asks for picky `=7.0.0-rc.26` instead of `=7.0.0-rc.25`, and the
macOS-only `curve25519-dalek`, `ed25519-dalek`, `p256`, `p384`, `p521` and
`primeorder` dependencies for their final releases instead of release
candidates. Cargo resolves macOS dependencies on Linux too, and russh (for SSH)
needs those final releases. picky rc.26 is the first to use them; winscard
0.3.4 pins it. No code changes were needed.

Remove this part when an sspi release depends on picky rc.26 or later.
