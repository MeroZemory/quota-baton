# Documentation map

| Document | Role | Read when |
|---|---|---|
| [decisions.md](decisions.md) | Records each decision and who made it | Before you change scope or direction |
| [architecture.md](architecture.md) | Explains how the system is built and why, including the verification plan and the milestones | When you first start on the project |
| [contract.md](contract.md) | Normative externally observable behaviour: routes, headers, body invariance, state machine, error mapping, selection, and file formats | Before implementing or testing any behaviour |

Rules:
- A normative statement lives in exactly one place. Behaviour belongs in `contract.md`; design rationale belongs in `architecture.md`.
- When two documents conflict, stop and resolve the conflict in the document that owns the statement.
- Research evidence lives under `.sandbox/research/`. It is git-ignored, may hold sensitive local detail, and is never normative.
