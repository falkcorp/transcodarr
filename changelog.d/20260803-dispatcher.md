### Added

#### `Dispatcher` — two-stage matching

Matching splits into eligibility and admission, and the split is why dispatch
stays O(agents) rather than O(queue × agents):

- **Eligibility is computed once per requirement bucket**, not once per job.
  Jobs sharing a `bucket_key` are interchangeable as far as agent capability
  goes, so the `satisfies` call runs once and the answer is cached. Observed
  bucket count for this environment is around eight, against tens of thousands
  of queued jobs.
- **Admission is per job**, covering exactly what was deliberately kept out of
  the bucket key: free bytes, effective cores and mount coverage. Those carry
  per-file numbers and paths, so folding them into the key would drive it toward
  one bucket per job and collapse the whole scheme (flaw A5).

Changing the fleet clears the eligibility cache — otherwise an agent that just
lost its GPU would keep being handed GPU work from a stale entry.

A **commit-ineligible agent is never chosen.** A node that failed the Phase 0
`RenameProbe` may produce output but must never install it; handing it work it
cannot finish is worse than leaving it idle.

Every refusal is recorded with a reason, and distinguishes **capability** ("no
agent can ever run this" — a problem to fix) from **capacity** ("every capable
agent is busy" — a queue that drains on its own). Carrying the reason is the
point: "no agent available" sends an operator hunting, "requires
`Encoder(HevcNvenc)`, no agent advertises it" is actionable.
