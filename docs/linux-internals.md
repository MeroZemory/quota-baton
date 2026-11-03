# Linux service internals

For installation and daily use, see the [Linux guide](linux.md). This page keeps
lifecycle rules and historical qualification evidence for maintainers. The
[contract](contract.md) defines externally observable behavior.

## Managed lifecycle

With QB_SYSTEMD_MANAGED=1, validate LISTEN_PID=self and named listening stream
FDs starting at 3. Current packaging supplies data/admin/admin-unix; older
two-descriptor data/admin activation remains supported. Validate exact loopback
endpoints and the Unix socket's path, node identity, mode and group. Map
by name, never ordering across separate socket units. Reject malformed activation;
do not fall back to bind. Defaults here are 127.0.0.1:10200 and :10201. Emit real
READY=1 only after all supplied listeners and application initialization are ready.

Managed `serve --takeover` must dispatch early to a bounded authenticated HTTP
helper, not acquire the store lease or become a foreground server. Read only the
existing private admin token, without logging it; use loopback without proxies.
Record authenticated health.started_at and PID, request reload once, and succeed
only after started_at changes, PID stays equal and new readiness is real. Reject
overlap/stopping. The helper deadline is 55 seconds, below the unit's 90-second
reload timeout. A timeout reports failure; it does not imply cancellation.

The main process completes ordinary admission stop, accepted-work drain, scheduler
and login cancellation, durable shutdown refreshes and binding/replay/observability
flushes before exec. Retain the original validated activation FDs with CLOEXEC
during normal execution. Declared Unix box listeners have a separate retained
bank: freeze it after box work and durable cleanup finish, and pass its validated
FD manifest in `QB_BOX_LISTENERS`. Clear CLOEXEC only on the retained activation and box
bank descriptors immediately before exec. Server duplicates close; never call
`shutdown()` on a shared listening socket. Restore LISTEN_PID, the original activation count and
matching LISTEN_FDNAMES, retaining systemd credential/runtime environment.
Exec the absolute original argv[0], `/usr/local/bin/quota-baton`, with `serve`.
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
These rules apply to the managed Linux service. See the
[activation and exec implementation](../crates/qb-core/src/platform/unix/listener.rs)
and [managed reload helper](../crates/qb-core/src/platform/unix/service.rs).

## Socket lifetime and stop policy

Enable only the service. Its Requires/After and Sockets explicitly bring up all
three listeners. Each socket has one-way PartOf=service, propagating stop/restart but
not reload. No socket requires or orders itself after the service: sockets
already order before it, so such reverse ordering would create a cycle.
NonBlocking belongs on the service. No independent sockets.target enablement.

Do not add BindsTo=service, which would close listeners on spontaneous service
loss, or StopWhenUnneeded=yes, which could discard the bank during recovery.
Reload leaves socket units open; restart restarts them. Backlog=4096 is finite
and kernel-capped. This is not a guarantee for every connection or active stream.

The systemd 255 gate verified service-only stop under continuous connections:
both sockets became inactive and later connects did not reactivate the service.
Maintenance commands below explicitly stop all four units in one transaction.

The admin Unix socket is 0660, owned by the service user and `qb-admin`. Runtime
directory 0710 permits only that group to traverse it; state remains 0700.
The runtime-only descriptor walk accepts exactly owner-private 0700 or owner/
`qb-admin` 0710. Private storage policy is unchanged. Native `SO_PEERCRED` is
checked before a connection reaches axum: the service uid is allowed, otherwise
bounded reentrant NSS calls must find that uid in `qb-admin`. Host and Origin
checks are shared with TCP; the Unix path needs no bearer. Its graceful parent
is joined alongside the TCP admin parent before durable shutdown completes.

## Intentional stop and recovery

```bash
sudo systemctl stop quota-baton.service quota-baton-data.socket quota-baton-admin.socket quota-baton-admin-unix.socket
for unit in quota-baton.service quota-baton-data.socket quota-baton-admin.socket quota-baton-admin-unix.socket; do
  state=$(sudo systemctl show "$unit" -p ActiveState --value)
  test "$state" = inactive || exit 1
done
```

Both ports must refuse connections without reactivation; prove it in the actual
gate, including service-only stop. Also `sudo systemctl disable quota-baton.service`
to keep it off after boot. Resume with `sudo systemctl start quota-baton.service`;
its requirements start all three sockets.

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

## Login lifecycle

The managed CLI uses the authenticated admin API. Standalone login additionally
needs credential delivery and the account store's exclusive lease. The core
creates its own OAuth sessions; account stores are not transferred from a PC.

Codex device polling follows the provider interval and never extends the code's
expiry or automatically requests a new code. Before switching to paste login,
the CLI cancels and checks the old session. If it completed first, the CLI reports
success instead of starting another session. An explicit paste fallback gets a
new ten-minute budget.

Each paste session accepts one submission. State and PKCE checks are shared with
the browser flow; invalid or reused input requires a new login. Ctrl+C cancels the
pending session and releases a standalone CLI's store lease. On stdin EOF, the
CLI confirms cancellation before reporting that account information was unchanged;
if authentication completed first, it reports success. Login without
`--no-browser` retains the browser and loopback callback flow.

## Box identity and revocation

Enrollment establishes key identity independently of listener declarations. A
box-scoped lease also requires that box's key; neither another box nor an ordinary
global client can use it. The host retains the admin credential.

An authenticated box may send `x-qb-root-session` as a placement preference.
Normal eligibility and capacity rules still apply, and the header is removed
before forwarding upstream. A header alone never establishes box identity.

Revocation stops fresh admission and interrupts the revoked box's response
streams and upgrades. Already admitted durable credential writes finish under
the retained service lease. An idle shared HTTP keepalive may remain open;
every subsequent request authenticates again and is refused after revocation.

## Qualification and limits

The historical P2 fixture used systemd 255 and glibc 2.39. Reload retained the same PID and
InvocationID, changed authenticated health.started_at and loaded the replacement
installed pathname. Each loopback port had 2,380 successful connects and zero
failures over 25 seconds. The replacement used the same ELF plus an inert trailing
marker to isolate re-exec ownership; this does not certify schema downgrades,
arbitrary upgrades or uninterrupted active streams. The gate also verified crash
restart, missing-credential refusal/recovery and service-only stop under load.
These measurements cover the P2 fixture only. They do not establish real-server
login, per-box access or reboot acceptance.

Primary sources: [socket units](https://www.freedesktop.org/software/systemd/man/255/systemd.socket.html),
[unit dependencies](https://www.freedesktop.org/software/systemd/man/255/systemd.unit.html),
[service readiness/reload and socket flags](https://www.freedesktop.org/software/systemd/man/255/systemd.service.html),
[activation FDs](https://www.freedesktop.org/software/systemd/man/255/sd_listen_fds.html),
[credential/directory handling](https://www.freedesktop.org/software/systemd/man/255/systemd.exec.html),
[credential encryption](https://www.freedesktop.org/software/systemd/man/255/systemd-creds.html),
and [v255 socket stop-job check](https://github.com/systemd/systemd/blob/v255/src/core/socket.c).
