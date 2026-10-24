# Linux headless service

The P2 service is qualified on Ubuntu 24.04 with systemd 255 in a synthetic
container: startup, status, same-PID reload, crash restart and intentional stop.
Server login, per-box access and real-server reboot acceptance remain P3–P5.

## Managed lifecycle

With QB_SYSTEMD_MANAGED=1, validate LISTEN_PID=self, exactly two listening stream
FDs 3/4, distinct names data/admin and exact configured loopback endpoints. Map
by name, never ordering across separate socket units. Reject malformed activation;
do not fall back to bind. Defaults here are 127.0.0.1:10200 and :10201. Emit real
READY=1 only after both listeners and application initialization are ready.

Managed `serve --takeover` must dispatch early to a bounded authenticated HTTP
helper, not acquire the store lease or become a foreground server. Read only the
existing private admin token, without logging it; use loopback without proxies.
Record authenticated health.started_at and PID, request reload once, and succeed
only after started_at changes, PID stays equal and new readiness is real. Reject
overlap/stopping. The helper deadline is 55 seconds, below the unit's 90-second
reload timeout. A timeout reports failure; it does not imply cancellation.

The main process completes ordinary admission stop, accepted-work drain, scheduler
and login cancellation, durable shutdown refreshes and binding/replay/observability
flushes BEFORE exec. Retain original validated bank FDs 3/4, CLOEXEC during normal
execution; clear CLOEXEC only on these originals immediately before exec. Server
duplicates close; never shutdown() a shared listening socket. Restore LISTEN_PID,
LISTEN_FDS=2 and matching LISTEN_FDNAMES, retaining systemd credential/runtime env.
Exec the absolute ORIGINAL argv[0], `/usr/local/bin/quota-baton`, with `serve`.
Never use current_exe()/proc/self/exe after atomic replacement: it can refer to
the old deleted inode. New initialization must emit actual READY again.

Exec refusal after cleanup exits nonzero for Restart=on-failure; helper fails.
Failed initialization or fallback restart changing PID also fails the helper.
Inspect state after helper timeout: an accepted request can still be draining or
completing. The helper deadline never cancels durable cleanup. A 45-second reload
budget prohibits re-exec after that budget, but login cancellation, token-save
retry and durable binding/observation flush finish while the lease remains held
before failure is reported. Storage failure can therefore keep shutdown waiting;
fix the storage problem instead of forcing success. The unit's 35-minute stop
limit is an administrator kill boundary, not a guarantee that arbitrary failed
storage will recover. Never skip flushes to meet a timeout.
Standalone Linux behavior and Windows behavior remain unchanged.

## Socket lifetime and stop policy

Enable ONLY the service. Its Requires/After and Sockets explicitly bring up both
listeners. Each socket has one-way PartOf=service, propagating stop/restart but
not reload. Neither socket requires or orders itself after the service: sockets
already order before it, so such reverse ordering would create a cycle.
NonBlocking belongs on the service. No independent sockets.target enablement.

Do not add BindsTo=service, which would close listeners on spontaneous service
loss, or StopWhenUnneeded=yes, which could discard the bank during recovery.
Reload leaves socket units open; restart restarts them. Backlog=4096 is finite
and kernel-capped. This is not a guarantee for every connection or active stream.

The systemd 255 gate verified service-only stop under continuous connections:
both sockets became inactive and later connects did not reactivate the service.
Maintenance commands below explicitly stop all three units in one transaction.

## Installation after qualification

Run from a qualified release directory containing `./quota-baton` and
`packaging/linux/`. Use an Ubuntu 24.04-compatible artifact; a Debian 13 glibc 2.41
binary is not assumed compatible with Ubuntu 24.04 glibc 2.39. These first-time
account/credential steps must not replace an existing master key on upgrade.

```bash
sudo useradd --system --user-group --home-dir /var/lib/quota-baton \
  --no-create-home --shell /usr/sbin/nologin quota-baton
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

Exactly 32 RAW random bytes go directly to encryption: no hex/base64 encoding,
passphrase or added newline. Only ciphertext is staged. Linking refuses an
existing destination. Explicit host-key encryption works without TPM and does
not claim TPM protection; a production TPM policy needs tested provisioning.
Never use an absent-key fallback. Preserve the encrypted credential and host
secret under the deployment's backup policy. No plaintext enters argv or env.

```bash
sudo install -o root -g root -m 0755 ./quota-baton /usr/local/bin/quota-baton
sudo install -o root -g root -m 0644 packaging/linux/quota-baton.service \
  packaging/linux/quota-baton-data.socket packaging/linux/quota-baton-admin.socket \
  /etc/systemd/system/
sudo systemd-analyze verify /etc/systemd/system/quota-baton.service \
  /etc/systemd/system/quota-baton-data.socket \
  /etc/systemd/system/quota-baton-admin.socket
sudo systemctl daemon-reload
sudo systemctl enable --now quota-baton.service
for unit in quota-baton.service quota-baton-data.socket quota-baton-admin.socket; do
  sudo systemctl is-active --quiet "$unit" || exit 1
done
sudo systemctl show quota-baton.service -p MainPID -p NRestarts
sudo -u quota-baton env QB_HOME=/var/lib/quota-baton \
  /usr/local/bin/quota-baton status
```

Systemd creates private state/runtime directories mode 0700, UMask=0077, and
delivers the decrypted credential in its private credential directory. Do not
print keys/tokens or put them into command arguments. Never enable either socket
separately. The direct binary install above assumes no running installation.

## Binary upgrade and bounded reload

Only one administrator upgrades at a time. Use a qualified binary supporting the
same endpoint/configuration contract. Save the previous binary and atomically
replace its installed name on the same filesystem, never overwrite in place:

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

Equal PID alone proves nothing: native helper success requires changed authenticated
started_at and readiness. A binary replacement needs no daemon-reload. Changed
units/endpoints/policy require separately qualified maintenance stop/start; restart
is not a reload and does not promise socket continuity.

## Intentional stop and recovery

```bash
sudo systemctl stop quota-baton.service quota-baton-data.socket quota-baton-admin.socket
for unit in quota-baton.service quota-baton-data.socket quota-baton-admin.socket; do
  state=$(sudo systemctl show "$unit" -p ActiveState --value)
  test "$state" = inactive || exit 1
done
```

Both ports must refuse connections without reactivation; prove it in the actual
gate, including service-only stop. Also `sudo systemctl disable quota-baton.service`
to keep it off after boot. Resume with `sudo systemctl start quota-baton.service`;
its requirements start both sockets.

After failed-upgrade inspection, perform the full stop above before rollback:

```bash
sudo install -o root -g root -m 0755 /usr/local/bin/.quota-baton.previous \
  /usr/local/bin/.quota-baton.next
sudo mv -T -- /usr/local/bin/.quota-baton.next /usr/local/bin/quota-baton
if sudo systemctl is-failed --quiet quota-baton.service; then
  sudo systemctl reset-failed quota-baton.service
fi
sudo systemctl start quota-baton.service
for unit in quota-baton.service quota-baton-data.socket quota-baton-admin.socket; do
  sudo systemctl is-active --quiet "$unit" || exit 1
done
```

Keep key/state unchanged. The previous binary must understand current durable
data; this does not claim schema downgrade compatibility.

## Browserless account login

Run the CLI in the same service-user environment and data home as the running
core. It uses the authenticated admin API when the core is running; a standalone
login uses the same account store and still requires its exclusive lease.

```bash
quota-baton accounts login claude --no-browser
quota-baton accounts login codex --no-browser
```

Codex defaults to device-code login: open the printed verification URL on a
computer with a browser and enter the displayed user code. The core polls at the
server's interval and shows the remaining expiry, at most fifteen minutes. It
does not automatically request another code after expiry or a rate limit.

If device-code login is unavailable, enable it in your personal ChatGPT Security
settings, or ask your workspace admin to allow it. You can press Enter while
waiting to switch immediately to paste-redirect login; unavailable or rate-limited
starts also offer this fallback. The old login is cancelled and checked first.
If it completed just before cancellation, the CLI reports success instead of
starting another login.

For the Codex paste fallback, open its authorization URL and copy the final
`http://localhost:1455/auth/callback?...` address from the browser address bar,
even if that local page cannot connect, then paste it into the waiting CLI.
Claude continues to use its authorization URL and displayed code (including the
state suffix). Neither paste flow needs a callback listener on the headless core.
Treat pasted codes and final redirect URLs as secrets; do not put them into shell
commands, tickets or logs. The device user code is displayed only for this login;
access and refresh tokens are never printed.

Each paste login expires after ten minutes and accepts one submission. State and
PKCE checks are shared with the browser flow. Invalid or reused input requires a
new login. An explicitly requested paste fallback gets a new ten-minute budget;
device polling never extends the device code's expiry. Ctrl+C cancels the pending
login and releases a standalone CLI's account-store lease.
The usual login without `--no-browser` retains the browser and loopback callback
flow.

## Qualification and limits

The P2 fixture used systemd 255 and glibc 2.39. Reload retained the same PID and
InvocationID, changed authenticated health.started_at and loaded the replacement
installed pathname. Each loopback port had 2,380 successful connects and zero
failures over 25 seconds. The replacement used the same ELF plus an inert trailing
marker to isolate re-exec ownership; this does not certify schema downgrades,
arbitrary upgrades or uninterrupted active streams. The gate also verified crash
restart, missing-credential refusal/recovery and service-only stop under load.
Real-server login, per-box access and reboot acceptance remain separate.

Primary sources: [socket units](https://www.freedesktop.org/software/systemd/man/255/systemd.socket.html),
[unit dependencies](https://www.freedesktop.org/software/systemd/man/255/systemd.unit.html),
[service readiness/reload and socket flags](https://www.freedesktop.org/software/systemd/man/255/systemd.service.html),
[activation FDs](https://www.freedesktop.org/software/systemd/man/255/sd_listen_fds.html),
[credential/directory handling](https://www.freedesktop.org/software/systemd/man/255/systemd.exec.html),
[credential encryption](https://www.freedesktop.org/software/systemd/man/255/systemd-creds.html),
and [v255 socket stop-job check](https://github.com/systemd/systemd/blob/v255/src/core/socket.c).
