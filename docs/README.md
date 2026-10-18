# Documentation map

| Document | Role | Read when |
|---|---|---|
| [decisions.md](decisions.md) | Records each decision and who made it | Before you change scope or direction |
| [architecture.md](architecture.md) | Explains how the system is built and why, including the verification plan and the milestones | When you first start on the project |
| [contract.md](contract.md) | Normative externally observable behaviour: routes, headers, body invariance, state machine, error mapping, selection, and file formats | Before implementing or testing any behaviour |
| [user-guide.md](user-guide.md) | Korean user guide: install, tray, accounts, cutover with `wire`, troubleshooting. Non-normative. | Before using quota-baton day to day or running the cutover |
| [tray-v2.md](tray-v2.md) | Approved R5 tray visuals and interactions, including the later every-time credits confirmation decision. Supersedes the old tray layout in architecture §10. | Before changing tray presentation, input, theme or notification wording |
| [architecture-overview.html](architecture-overview.html) | Korean visual overview of the three documents above, for the owner. It is non-normative: where it differs, the documents above win. | To grasp the design quickly; open it in a browser |

Rules:
- A normative statement lives in exactly one place. Behaviour belongs in `contract.md`; design rationale belongs in `architecture.md`.
- When two documents conflict, stop and resolve the conflict in the document that owns the statement.
- Research evidence lives under `.sandbox/research/`. It is git-ignored, may hold sensitive local detail, and is never normative.
