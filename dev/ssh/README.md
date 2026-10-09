Dev-only SSH keys for the SSH server container (`scripts/dev-db.sh up ssh`) and the tunnel tests.
They are not secret and grant nothing outside that container.

- `id_ed25519`: no passphrase.
- `id_ed25519_passphrase`: passphrase `dbear-passphrase`.

The container's user is `dbear` with password `dbear`; both keys are authorized.
