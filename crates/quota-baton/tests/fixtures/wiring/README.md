Synthetic schema-3 journal shapes for R9.2 compatibility tests. They mirror the
released R3 entry shape, the R3.1 dedicated Claude/original-file fields, and the
R3.2 command/upgrade fields. No fixture is copied from a live journal. Paths,
hashes, file IDs and protected payloads are inert placeholders. Tests replace
only `home` with their project-local synthetic target.

The R3 file entry deliberately retains a historical `pending` path after
`done=true`; completed file publication could keep that metadata after deleting
the scratch name. An active `upgrade`, removed entry or restoring original is
different and must refuse. None of these fixtures has `install_completed`.

R3.2 also carries `claude_restored=true`: dedicated Claude upgrades restored
the prior default settings while leaving the dedicated Codex installation live.
This independent historical flag must not block Codex launch.
