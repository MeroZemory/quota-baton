# Linux headless service

quota-baton runs a shared Claude and Codex account proxy under systemd. The
supplied units keep the data port at `127.0.0.1:10200` and the host admin port at
`127.0.0.1:10201`. Authorized host users can also use
`/run/quota-baton/admin.sock` without reading the service's private admin token.

## Install

Use Ubuntu 24.04 or newer with systemd 255 or newer, and a release binary built
for your machine's architecture and glibc. A Debian 13 glibc 2.41 binary is not
assumed to run on Ubuntu 24.04 glibc 2.39. Qualification evidence and lifecycle
details are in [Linux service internals](linux-internals.md#qualification-and-limits).

Extract your downloaded release tarball and enter its directory. Replace the
example filename with your artifact's name; the directory must contain
`quota-baton` and `packaging/linux/`.

```bash
tar -xzf /path/to/quota-baton-release.tar.gz
cd /path/to/extracted-release
```

For a first installation, create the service user and encrypt a new master key.
Do this once: keep the existing key and state when upgrading.

```bash
sudo useradd --system --user-group --home-dir /var/lib/quota-baton \
  --no-create-home --shell /usr/sbin/nologin quota-baton
sudo groupadd --system qb-admin
sudo install -d -o quota-baton -g quota-baton -m 0700 /var/lib/quota-baton
sudo install -d -o root -g root -m 0700 /etc/credstore.encrypted
sudo systemd-creds setup
sudo bash -euo pipefail <<'QB_PROVISION'
umask 077
candidate_dir=$(mktemp -d /etc/credstore.encrypted/.qb-master-key.XXXXXX)
trap 'rm -f -- "$candidate_dir/qb-master-key"; rmdir -- "$candidate_dir"' EXIT
openssl rand 32 | systemd-creds encrypt --name=qb-master-key --with-key=host \
  - "$candidate_dir/qb-master-key"
chmod 0600 "$candidate_dir/qb-master-key"
ln -- "$candidate_dir/qb-master-key" /etc/credstore.encrypted/qb-master-key
QB_PROVISION
```

The key is exactly 32 raw random bytes, with no encoding or newline. Only
ciphertext is staged, and the final link refuses to replace an existing key.
Host-key encryption works without a TPM; it does not provide TPM protection.
Back up the encrypted credential, the systemd host secret and service state
under your deployment's backup policy. Keep keys and tokens out of arguments,
environment variables and logs.

Install the binary and units, then start the service:

```bash
sudo install -o root -g root -m 0755 ./quota-baton /usr/local/bin/quota-baton
sudo install -o root -g root -m 0644 packaging/linux/quota-baton.service \
  packaging/linux/quota-baton-data.socket packaging/linux/quota-baton-admin.socket \
  packaging/linux/quota-baton-admin-unix.socket \
  /etc/systemd/system/
sudo systemd-analyze verify /etc/systemd/system/quota-baton.service \
  /etc/systemd/system/quota-baton-data.socket \
  /etc/systemd/system/quota-baton-admin.socket \
  /etc/systemd/system/quota-baton-admin-unix.socket
sudo systemctl daemon-reload
sudo systemctl enable --now quota-baton.service
```

Enable only the service; it starts all three sockets. Systemd keeps state in
`/var/lib/quota-baton` (0700) and delivers the decrypted key through its credential
directory. `/run/quota-baton` is 0710 with group `qb-admin`: the group can traverse
to the 0660 admin socket, but cannot list or change the runtime directory.
The direct binary install above assumes there is no running installation.

Check all four units and the application:

```bash
for unit in quota-baton.service quota-baton-data.socket quota-baton-admin.socket quota-baton-admin-unix.socket; do
  sudo systemctl is-active --quiet "$unit" || exit 1
done
sudo systemctl show quota-baton.service -p MainPID -p NRestarts
sudo -u quota-baton env QB_HOME=/var/lib/quota-baton \
  /usr/local/bin/quota-baton status
```

## Allow a host orchestrator to administer the service

Add only trusted administrators to `qb-admin`; membership grants the same admin
API access as the private bearer token. For the server orchestrator:

```bash
sudo usermod -aG qb-admin agentsrv
sudo -u agentsrv /usr/local/bin/quota-baton status
sudo -u agentsrv curl --fail --unix-socket /run/quota-baton/admin.sock \
  http://127.0.0.1:10201/admin/v1/health
```

Restart an already-running orchestrator or log in again to pick up its new group.
The CLI automatically uses the socket for the managed default endpoint. Peer
credentials authorize the service user or a user in `qb-admin`; no token is read
from `/var/lib/quota-baton`. The TCP admin endpoint still requires its bearer
token. Keep `Host` as `127.0.0.1:10201` or `localhost:10201`, including for curl
requests over the Unix socket.

## Upgrade

Use a compatible release with the same endpoint configuration. Have one
administrator perform the upgrade at a time. From the new release directory,
save the old binary and replace the installed name atomically on the same
filesystem:

```bash
sudo cp --preserve=mode,ownership,timestamps -- /usr/local/bin/quota-baton \
  /usr/local/bin/.quota-baton.previous
sudo install -o root -g root -m 0755 ./quota-baton /usr/local/bin/.quota-baton.next
sudo mv -T -- /usr/local/bin/.quota-baton.next /usr/local/bin/quota-baton
before_pid=$(sudo systemctl show quota-baton.service -p MainPID --value)
test "$before_pid" -gt 0 || exit 1
sudo systemctl reload quota-baton.service || exit 1
after_pid=$(sudo systemctl show quota-baton.service -p MainPID --value)
test "$before_pid" = "$after_pid" || exit 1
sudo -u quota-baton env QB_HOME=/var/lib/quota-baton \
  /usr/local/bin/quota-baton status
```

Reload success checks that the new application is ready with the same PID.
Listeners stay open, but active streams and every connection are not guaranteed
to survive. A binary swap needs no `daemon-reload`. Unit, endpoint or policy
changes need a separately validated maintenance stop/start. In particular, adding
the admin Unix socket unit requires a stop/start to inherit its third descriptor;
a binary-only reload of an older two-socket service keeps its existing TCP
endpoints and does not invent a third listener. If reload fails,
inspect the service before retrying; see [recovery](linux-internals.md#intentional-stop-and-recovery).

## Log in without a server browser

Start the service first. Run login as the service user with its data home:

```bash
sudo -u quota-baton env QB_HOME=/var/lib/quota-baton \
  /usr/local/bin/quota-baton accounts login claude --no-browser
sudo -u quota-baton env QB_HOME=/var/lib/quota-baton \
  /usr/local/bin/quota-baton accounts login codex --no-browser
```

For Claude, open the printed authorization URL on a computer with a browser.
Paste the displayed code, including its state suffix, into the waiting CLI.

Codex defaults to device-code login. Open the printed verification URL on a
computer with a browser and enter the user code. Complete login before the
shown expiry, at most fifteen minutes. No new code is requested automatically
after expiry or a rate limit.

If device-code login is unavailable, enable it in your personal ChatGPT Security
settings or ask your workspace admin to allow it. Press Enter while waiting to
switch to paste-redirect login; unavailable or rate-limited starts also offer
this fallback. Open the fallback authorization URL, then copy the final
`http://localhost:1455/auth/callback?...` address from the browser address bar,
even if that page cannot connect. Paste it into the waiting CLI.

Paste login needs no callback listener on the server. It expires after ten
minutes and accepts one submission; invalid or reused input needs a new login.
Ctrl+C cancels a pending login. Treat codes and final redirect URLs as secrets;
do not paste them into shell commands or logs. The server creates its own
sessions: do not copy a PC's account store, and leave any designated PC-only
Codex login on that PC.

## Give each box its own key

Use the authenticated host admin API from your orchestrator. Enrollment needs
only a name; a listener declaration is optional.

| Method and path | Operation |
| --- | --- |
| `GET /admin/v1/boxes` | List names, epochs and revocation state, without keys. |
| `POST /admin/v1/boxes` | Enroll `{"name":"sample-box"}` and return its key once. |
| `POST /admin/v1/boxes/sample-box/launches` | Register a Codex launch with the usual launch options and return its scoped lease URL. |

Keep the enrollment response private. Deliver only that box's key and scoped
lease URL to it; keep the admin credential on the host. A box-scoped lease also
requires its box's key. Claude uses its box key on the regular data routes.
A key-only box can use the existing loopback data port through a trusted proxy.
Keep both host ports private; the admin port always requires the host credential.

For an Incus system container, add this optional listener to
`/var/lib/quota-baton/config.toml` before enrollment. Apply the configuration
with a maintenance stop/start using the [stop steps](linux-internals.md#intentional-stop-and-recovery):

```toml
[[boxes]]
name = "sample-box"
listen = "unix"
```

When the name is enrolled, the managed service creates
`/run/quota-baton/boxes/sample-box.sock`, which accepts only that box's key.
On the Incus host, add the proxy device:

```bash
incus config device add sample-box qb proxy \
  listen=tcp:127.0.0.1:10200 \
  connect=unix:/run/quota-baton/boxes/sample-box.sock bind=instance
```

`bind=instance` puts the TCP listener inside the container. Its client uses
`http://127.0.0.1:10200` and sends its own key. Keep the visible port equal to the
configured data port for Host validation. Per-box `listen = "tcp:127.0.0.1:<port>"`
is reserved for VM support and is refused by this build.

Create or replace `config.toml` with explicit private permissions, rather than a
new file through `tee` under the caller's umask:

```bash
sudo install -o quota-baton -g qb-admin -m 0600 ./config.toml \
  /var/lib/quota-baton/config.toml
```

For Codex, register the launch with an explicit supported `model` when it has no
prior binding. Use the returned scoped lease `base_url`. Fetch its model catalog
using the box key and a `?client_version=` matching the box's Codex CLI version,
and save the returned catalog to a private local file. Keep enrollment and launch
responses out of shared logs.

Inside the box, sign in with its API key through standard input:

```bash
read -r -s -p 'Box key: ' box_key; printf '\n'
printf '%s' "$box_key" | codex login --with-api-key
unset box_key
codex -c 'openai_base_url="<scoped lease base_url>"' \
  -c 'model_catalog_json="/private/path/to/catalog.json"' -m '<supported model>'
```

Replace the placeholders with the launch response, saved catalog and selected
model. A bare `OPENAI_API_KEY` is insufficient for the custom Codex base URL.
For Claude Code, set `ANTHROPIC_BASE_URL=http://127.0.0.1:10200` and
`ANTHROPIC_API_KEY` to that box's key in its private client environment. Never put
the host admin token inside a box.

## Revoke a box

Send `POST /admin/v1/boxes/sample-box/revoke` through the authenticated host admin
API. This revokes the current epoch, blocks new requests and interrupts its
active response streams and upgrades. Already admitted durable credential writes
finish. An idle keepalive may stay open, but subsequent requests are refused.
A revoked name cannot be enrolled again; use a new box name for its replacement.

## Uninstall

Stop all four units and disable the service before removing its files:

```bash
sudo systemctl stop quota-baton.service quota-baton-data.socket quota-baton-admin.socket quota-baton-admin-unix.socket || exit 1
sudo systemctl disable quota-baton.service
sudo rm -f -- /etc/systemd/system/quota-baton.service \
  /etc/systemd/system/quota-baton-data.socket \
  /etc/systemd/system/quota-baton-admin.socket \
  /etc/systemd/system/quota-baton-admin-unix.socket
sudo systemctl daemon-reload
sudo rm -f -- /usr/local/bin/quota-baton \
  /usr/local/bin/.quota-baton.previous /usr/local/bin/.quota-baton.next
```

If you added the example Incus device, remove it with
`incus config device remove sample-box qb`. These steps retain the service user,
`/var/lib/quota-baton` and `/etc/credstore.encrypted/qb-master-key` for recovery.
Delete them separately only when you intend to discard the stored accounts.
Do not delete a shared systemd host secret.

## Troubleshooting

```bash
sudo systemctl status quota-baton.service quota-baton-data.socket quota-baton-admin.socket quota-baton-admin-unix.socket
sudo journalctl -u quota-baton.service -n 100 --no-pager
```

If startup fails, check the release's architecture/glibc compatibility, the unit
verification output and credential provisioning. A missing credential must fail
startup; do not generate a replacement key for an existing account store.

If reload times out, inspect the journal and application status before retrying.
The accepted reload may still be draining or saving state. Fix storage failures
so durable writes can finish. See [reload internals](linux-internals.md#managed-lifecycle)
and the [stop and rollback steps](linux-internals.md#intentional-stop-and-recovery).
Rollback requires a binary that understands the current durable data.

If a box cannot connect, check enrollment, its listener declaration, the Incus
device's `bind=instance` setting, the visible data port and the box's own key.
For login problems, try the Codex paste fallback or start a new paste session
after invalid input or expiry. Keep secrets out of any shared diagnostic output.
