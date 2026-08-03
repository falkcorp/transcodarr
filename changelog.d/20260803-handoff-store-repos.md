### Changed

#### Handoff records the read pool and repositories

Phase 2 status now lists `ReadPool` and the four implemented repositories as
done, and `Scanner`, `Evaluator` and `admin explain` as what remains — with the
`FileRepo`/`LibraryRepo` calls each will use. Also records the seven
repositories deliberately not written yet, and the open question of whether the
CLI links the store or calls into the server for `admin explain`.
