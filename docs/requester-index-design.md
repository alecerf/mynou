# Requester index delivery

This design describes the requester-ledger increment of [growing inventories](https://github.com/alecerf/mynou/issues/106). It is implemented through [issue 186](https://github.com/alecerf/mynou/issues/186), covering the ledger and its direct consumers. Parent issue 106 retains the remaining scheduling, upstream paging, IRC inventory and diagnostics work.

The ledger owns an `inventory::Index` under the existing jobs-directory lock. Accounts remain a bounded configuration cache. Demands and operator interests have primary keys; account, media, admission, outcome and IRC-origin references have secondary keys. Primary records, references, counters and revision fences change in the same bounded transaction. Reads return explicit errors, including unreadable ownership proofs.

Legacy state is exposed through the same fallible point/page read interface for migration preflight. Both requester/job provenance and requester/IRC origin relationships must pass before publication. Exact legacy bytes and interrupted staging are retained. Publication binds a marker to the verified index identity; downgrade restores pre-migration state and cannot retain later changes.

Runtime updates use explicit bounded patches, never a partially loaded object that pretends to represent the entire ledger. Reservation is durable before job creation. Outcome reconciliation and admission resume through indexed cursors; predicates obtain authoritative point or complete media-interest answers. Policy revisions fence new admission without rewriting every unadmitted record during a preview. Successful upstream generations establish origin removal; a partial or failed poll cannot imply absence.

CLI and API pages carry continuation cursors and counts. Preview tokens bind ledger/jobs sequences, affected account/demand and captured configuration. Per-record, parser, network-worker and batch limits remain; retained inventory has no application-wide total limit. All runtime, recovery and growth evidence comes from GitHub Actions, with distinct QA and Security review before delivery.
