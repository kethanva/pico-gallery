# PicoGallery design-note archive

These files are tracked so architectural history is available in every clone.
They are **not active implementation plans**. Current requirements live in
`../spec.md`; current work and checkbox status live in `../plan.md`.

| File | Status | Purpose |
|---|---|---|
| `feature_new.md` | Proposal | Candidate configuration and system enhancements. |
| `photoprism_api_plugin_review.md` | Research | PhotoPrism API capabilities relevant to the Rust plugin. |
| `photoprism_grpc_implementation.md` | Historical alternative | gRPC sidecar design; not the current plugin architecture. |
| `photoprism_sql_nfs_implementation.md` | Historical alternative | Direct SQL/NFS design; not the current plugin architecture. |
| `pi_zero_rust_features_research.md` | Research backlog | Pi Zero feature ideas, not committed scope. |
| `split_client_server_implementation.md` | Historical alternative | Split-client/server proposal; PicoGallery remains a direct-rendering Rust appliance. |
| `walkthrough.md` | Implementation background | Performance and image-quality rationale; verify details against current code. |

When a proposal is adopted, move its requirements into `spec.md`, track the
work in `plan.md`, and leave only the decision rationale here.
