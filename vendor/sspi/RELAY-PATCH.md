# Relay patches to sspi 0.21.3

The CredSSP client always sends TSRequest version 6. From its pubKeyAuth
message on, it also sent the `clientNonce` field ([5]) whatever version the
server spoke. Servers older than CredSSP 5 don't define that field. Windows
Server 2008 (CredSSP 2) resets the connection when it arrives, so NLA sign-in
to it failed with "Connection reset by peer". The client now sends the nonce
only to peers of version 5 or later, as FreeRDP does. The public key proof
already used the version-appropriate method (echo below 5, hash from 5).

Remove this patch when an sspi release does the same.
