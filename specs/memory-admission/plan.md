# Implementation Plan: Memory Admission Ledger

## Goal

Add a process-wide byte admission ledger above mimalloc. The ledger will reserve memory before a bounded standing-root artifact slice starts. It will yield maintenance work when the configured ceiling cannot admit that reservation. It will not delay or reject interactive reader work. The ledger will use RAII reservations so every return, error, panic unwind, and cancellation releases the charge. Existing RSS, physical-footprint, SQLite, mapped-file, and subsystem telemetry remains authoritative for observed process residency.

## Technical Context

**Language**: Rust 2021 workspace
**Allocator**: mimalloc from the parent `memory-mimalloc` branch
**Scheduler**: process-wide standing-root deficit round-robin scheduler
**Existing concurrency gate**: `ColdBuildLimiter` limits build count, not bytes
**Configuration ownership**: user tier only
**Constraints**:

- Do not infer a ceiling from host RAM or a percentage.
- Do not add an unmeasured tuning constant.
- Derive each reservation from the selected slice inputs and concrete element sizes.
- Treat the ledger as a logical owner budget for known artifact work, not as mimalloc accounting.
- Charge known native-library requests when AFT controls their size, even if the bytes bypass Rust `GlobalAlloc`.
- Do not count SQLite page-cache or memory-mapped file residency when AFT cannot reserve it before allocation.
- Do not replace RSS, physical-footprint, SQLite, mapped-file, or subsystem telemetry.
- Do not route interactive readers through the admission ledger.
- Do not couple admission to allocator collection.

## Project Structure

```text
crates/aft/src/
├── memory_admission.rs          # New process-wide ledger, reservation guard, snapshot, tests
├── lib.rs                       # Register the memory admission module
├── config.rs                    # Add MemoryConfig and optional limit_bytes
├── config_resolve.rs            # Resolve user-tier memory config and strip project-tier weakening
├── context.rs                   # Store the shared ledger on App and expose it through AppContext
├── search_index.rs              # Compute and reserve the next search slice payload
├── semantic_index.rs            # Compute and reserve collect/embed slice payloads
├── subc/standing.rs             # Apply reservations before standing slice allocation
└── subc/health.rs               # Report admission limit, charged bytes, peak, and denials
packages/opencode-plugin/src/
├── config.ts                    # Parse memory.limit_bytes
└── __tests__/config.test.ts     # Verify schema and user/project ownership
packages/pi-plugin/src/
├── config.ts                    # Parse memory.limit_bytes
└── __tests__/config.test.ts     # Verify schema and user/project ownership
docs/config.md                   # Document the explicit ceiling and disabled default
```

## Operational Scenarios

### Scenario 1 — Reserve bounded maintenance memory

A standing-root search or semantic slice computes the bytes owned by its next bounded unit before it allocates that unit. The shared ledger admits the slice only when the reservation fits within the configured process ceiling.

**Acceptance**

Given a configured ceiling and enough remaining capacity, when a standing artifact slice requests a reservation, then the ledger returns an owned reservation for the requested bytes.

- [ ] The ledger atomically increments charged bytes.
- [ ] The reservation carries the artifact class and root identity for diagnostics.
- [ ] Dropping the reservation atomically releases exactly the admitted bytes.
- [ ] Zero-byte reservations succeed without changing counters.
- [ ] Integer overflow fails closed.

**Independent test**: unit tests in `crates/aft/src/memory_admission.rs`.

**Failure modes**: concurrent reservations, panic unwind, duplicate drop, byte-counter overflow.

**Depends on**: none.

### Scenario 2 — Yield denied maintenance without allocation

A standing artifact slice that does not fit must yield to the scheduler. The slice must not start its payload allocation or hold the cold-build permit beyond the current job.

**Acceptance**

Given a configured ceiling with insufficient remaining capacity, when a standing search or semantic slice requests its reservation, then the slice returns a successful yielded result without running the allocation closure.

- [ ] Charged bytes remain unchanged.
- [ ] The denial counter increments once.
- [ ] The denial records the requested bytes and artifact class.
- [ ] The scheduler keeps the root eligible for a later rotation.
- [ ] The cold-build permit releases through its existing drop guard.

**Independent test**: standing scheduler contract tests with allocation observers.

**Failure modes**: concurrent releases, cancellation before admission, stale standing publication epoch.

**Depends on**: Scenario 1.

### Scenario 3 — Preserve unlimited default behavior

AFT must retain its current behavior unless the user sets an explicit ceiling.

**Acceptance**

Given no `memory.limit_bytes` user setting, when any standing slice requests a reservation, then the ledger admits it without a byte ceiling.

- [ ] Existing configurations deserialize unchanged.
- [ ] Project configuration cannot set or weaken the process ceiling.
- [ ] Both plugin schemas accept the same positive integer domain.
- [ ] Zero, negative, fractional, and unsafe JavaScript integers are rejected.

**Independent test**: Rust config resolver tests and both plugin config suites.

**Failure modes**: project-tier override, JSON number precision loss, default drift between TypeScript and Rust.

**Depends on**: Scenario 1.

### Scenario 4 — Report admission state honestly

Health diagnostics must expose policy state independently from observed process residency.

**Acceptance**

Given an enabled or disabled ledger, when the health rollup refreshes, then the memory section reports admission state from one lock-free snapshot.

- [ ] The payload reports `limit_bytes`, `charged_bytes`, `peak_charged_bytes`, `available_bytes`, and `denied_total`.
- [ ] An absent ceiling serializes as disabled, not as zero capacity.
- [ ] `charged_bytes` never exceeds `limit_bytes` when a limit exists.
- [ ] Existing RSS, physical-footprint, allocator, SQLite, and attributed-byte fields remain unchanged.

**Independent test**: health payload tests in `crates/aft/src/subc/health.rs`.

**Failure modes**: concurrent snapshot, saturated counters, omitted root detail.

**Depends on**: Scenario 1.

## Changes

### Process-wide ledger

- **File**: `crates/aft/src/memory_admission.rs`
- **Change**: Add `MemoryAdmissionLedger`, `MemoryReservation`, `MemoryAdmissionClass`, `MemoryAdmissionError`, and `MemoryAdmissionSnapshot`.
- **Behavior**: Store the optional limit, current charge, peak charge, denial count, and last denied request in atomics plus a small diagnostic lock. Use a compare-exchange loop to admit a request without oversubscription. Release through `Drop` with checked debug assertions.
- **Test**: Verify concurrent admission never exceeds the limit. Verify RAII release after normal return and panic unwind. Verify denied work never executes its allocation closure.

### Shared process ownership

- **File**: `crates/aft/src/context.rs:2142`
- **Change**: Construct one `Arc<MemoryAdmissionLedger>` on the process-wide `App`. Expose a clone through `AppContext`. Reconfigure the optional ceiling from the resolved user snapshot without replacing the ledger or losing live charges.
- **Reuses**: The process-wide ownership pattern used by `cold_build_limiter()` in `crates/aft/src/context.rs:5346`.

### Configuration

- **File**: `crates/aft/src/config.rs:93`
- **Change**: Add `MemoryConfig { limit_bytes: Option<u64> }` and `Config::memory`. Default to `None`.
- **File**: `crates/aft/src/config_resolve.rs:516`
- **Change**: Add `RawMemory`. Accept only a positive user-tier `limit_bytes`. Strip the full `memory` section from project-tier configuration because a repository must not control the host process ceiling.
- **File**: `packages/opencode-plugin/src/config.ts:119`
- **Change**: Add an optional user-facing `memory.limit_bytes` positive safe-integer schema.
- **File**: `packages/pi-plugin/src/config.ts:126`
- **Change**: Add the same schema and TypeScript interface.
- **Test**: Assert Rust and TypeScript defaults, accepted bounds, rejected values, and project-tier stripping.

### Search slice reservation

- **File**: `crates/aft/src/search_index.rs:937`
- **Change**: Preserve the existing full corpus inventory and fingerprint preparation. Classify that path-list state as uncovered. After the existing cursor selects the exact next path range, derive a conservative bound for file content, trigram maps, `SpillRecord` values, and staging records from selected file metadata capped by `max_file_size`. Reserve before `prepare_search_path` reads content or creates trigram maps.
- **File**: `crates/aft/src/subc/standing.rs:607`
- **Change**: Admit the selected execution range through the shared ledger. Return `(false, true)` without invoking payload preparation when denied.
- **Test**: Use a slice observer to prove denial occurs before selected file content or trigram maps allocate. Verify telemetry reports corpus inventory as uncovered.

### Semantic slice reservation

- **File**: `crates/aft/src/semantic_index.rs:2849`
- **Change**: Preserve the existing whole-manifest staging load and classify its deserialized chunks, prior vectors, semantic collection, and local model state as uncovered. After the existing embed cursor selects a bounded batch, derive the incremental reservation for new embedding vectors from batch length, model dimension, and `size_of::<f32>()`. Reserve before invoking the embedding callback and before allocating the new vectors.
- **File**: `crates/aft/src/subc/standing.rs:663`
- **Change**: Admit the incremental vector result after staging load and batch selection. Yield without invoking the embedding callback when denied. Do not claim the reservation covers whole-manifest deserialization, duplicated batch text references, collection, or `SemanticEmbeddingModel::from_config` native model state.
- **Test**: Prove denial occurs before the embedding callback and new vector allocation. Verify telemetry reports the staging manifest, collection, and local model initialization as uncovered.

### Callgraph boundary

- **File**: `crates/aft/src/callgraph_store/mod.rs:715`
- **Change**: Add the ledger integration contract at `with_cold_build_slice_budget`, but charge only heap-owned extraction/indexing dispatch windows. Do not charge SQLite page-cache or database-file bytes. Derive each reservation from the existing bounded file window and extracted record representation.
- **File**: `crates/aft/src/subc/standing.rs:453`
- **Change**: Wire the callgraph reservation when the standing callgraph slice runner is active. If this branch still leaves standing callgraph execution intentionally yielded, keep the ledger API covered at the callgraph builder seam and do not fabricate a new callgraph scheduler path in this PR.
- **Test**: Verify a denied callgraph window does not enter extraction and does not count SQLite bytes as a heap charge.

### Health telemetry
- **File**: `crates/aft/src/subc/health.rs:715`
- **Change**: Extend `memory_rollup_metrics` to accept an explicit process-wide `MemoryAdmissionSnapshot`. Update the normal rollup, busy executor sample, and `HealthDiagnosticRollup::unavailable` call paths so all memory payload states contain the same `admission` object. The unavailable path emits a shape-compatible unavailable snapshot rather than reaching through global state. Keep every existing memory field byte-for-byte compatible.
- **Test**: Cover disabled, admitted, denied, released, busy, and unavailable payload states. Assert the existing field names and byte units remain unchanged.

### Documentation

- **File**: `docs/config.md`
- **Change**: Document `memory.limit_bytes` as an optional user-only process ceiling. Explain that it governs known heap reservations for cold maintenance slices. State that it is not an RSS hard limit and does not include SQLite, memory maps, thread stacks, native libraries, or child processes.

## Task List

### Phase 1 — Ledger foundation

- [ ] T001 Test `MemoryAdmissionLedger` admission, denial, concurrent ceiling, overflow, and RAII release — `crates/aft/src/memory_admission.rs`
- [ ] T002 Implement the ledger and reservation guard — `crates/aft/src/memory_admission.rs`
- [ ] T003 Register the module and add one process-wide ledger to `App` — `crates/aft/src/lib.rs`, `crates/aft/src/context.rs`

**Checkpoint**: The ledger is independently testable and cannot oversubscribe its configured ceiling.

### Phase 2 — Configuration

- [ ] T004 Test Rust defaults, user-tier resolution, invalid values, and project-tier stripping — `crates/aft/src/config_resolve.rs`
- [ ] T005 Implement `MemoryConfig` and resolver support — `crates/aft/src/config.rs`, `crates/aft/src/config_resolve.rs`
- [ ] T006 Test OpenCode and Pi schema parity — `packages/opencode-plugin/src/__tests__/config.test.ts`, `packages/pi-plugin/src/__tests__/config.test.ts`
- [ ] T007 Implement both plugin schemas and interfaces — `packages/opencode-plugin/src/config.ts`, `packages/pi-plugin/src/config.ts`

**Checkpoint**: The ceiling is explicit, user-only, optional, and identical across all configuration surfaces.

### Phase 3 — Search admission

- [ ] T008 Test that a denied search slice does not enter selected file payload preparation — `crates/aft/src/search_index.rs`, `crates/aft/src/subc/standing.rs`
- [ ] T009 Add selected-range search planning with formula-derived content, trigram, and staging bytes — `crates/aft/src/search_index.rs`
- [ ] T010 Reserve before selected search range execution and report corpus inventory as uncovered — `crates/aft/src/subc/standing.rs`

**Checkpoint**: Search maintenance charges the bounded execution payload. Corpus inventory remains explicitly uncovered.

### Phase 4 — Semantic admission

- [ ] T011 Test embedding denial before callback and new vector allocation — `crates/aft/src/semantic_index.rs`
- [ ] T012 Add incremental embedding-result planning from batch length, dimension, and `f32` size — `crates/aft/src/semantic_index.rs`
- [ ] T013 Reserve before embedding execution and report whole-manifest, collection, and model-init gaps — `crates/aft/src/subc/standing.rs`


### Phase 5 — Callgraph admission boundary

- [ ] T014 Test denied callgraph extraction windows and SQLite exclusion — `crates/aft/src/callgraph_store/mod.rs`
- [ ] T015 Add heap-window reservations at the bounded callgraph slice seam — `crates/aft/src/callgraph_store/mod.rs`
- [ ] T016 Wire the standing callgraph runner only if its execution path already exists on the parent branch — `crates/aft/src/subc/standing.rs`

**Checkpoint**: The callgraph builder exposes the same reservation contract without adding unrelated standing-callgraph behavior.

### Phase 6 — Telemetry and documentation

- [ ] T017 Test health output for disabled, admitted, denied, released, busy, and unavailable states — `crates/aft/src/subc/health.rs`
- [ ] T018 Thread an explicit admission snapshot through every `memory_rollup_metrics` caller and preserve the existing fields — `crates/aft/src/subc/health.rs`
- [ ] T019 Document the ceiling, coverage, exclusions, and disabled default — `docs/config.md`
**Checkpoint**: Operators can distinguish configured capacity, live reservations, denial pressure, and observed residency.

### Phase 7 — Verification and delivery

- [ ] T020 Run focused Rust ledger, config, standing, search, semantic, callgraph, and health tests.
- [ ] T021 Run both plugin config suites.
- [ ] T022 Run `cargo fmt --all -- --check` and repository diagnostics.
- [ ] T023 Run the full Rust regression suite and release storm gate.
- [ ] T024 Commit on `memory-admission` and create a draft PR stacked on `memory-mimalloc` / PR #285.

## Dependencies And Execution Order

- The ledger foundation blocks every integration phase.
- Configuration depends on the ledger API because reconfiguration must preserve live reservations.
- Search and semantic integration can proceed independently after configuration.
- The callgraph boundary depends on the ledger but not on search or semantic integration.
- Health telemetry depends only on the ledger snapshot API.
- Documentation follows the final public field names and measured coverage.

## Edge Cases And Risks

- **Risk: limit decreases below current charges.** Reject new reservations. Keep existing guards valid. Report zero available bytes until releases reduce the charge below the limit.
- **Risk: configuration reload replaces accounting state.** Update the existing ledger limit atomically. Never replace the ledger while reservations exist.
- **Risk: logical estimates undercount allocator overhead.** Define charges as formula-derived owned payload bounds. Keep observed RSS as the independent process check. Do not claim the configured value is an RSS hard limit.
- **Risk: planning allocates the payload it intends to guard.** Admit only after existing inventory or manifest state selects a bounded execution batch. Do not claim that the ledger covers the prerequisite state.
- **Risk: search inventory materializes before slice selection.** Report the corpus path list and fingerprint state as uncovered. Admit selected file content, trigram, and staging payloads only.
- **Risk: semantic staging materializes before batch selection.** Report the whole manifest, prior vectors, duplicated strings, collection, and local model state as uncovered. Admit incremental new vector results only.
- **Risk: a denied root spins every 250 ms.** Reuse DRR rotation and resource-pause telemetry. Record denials. Do not add a second timer or retry loop.
- **Risk: native allocations bypass mimalloc.** Keep the ledger allocator-neutral. Charge known logical ownership before controlled native-library calls. Keep native and process telemetry as the independent residency check.
- **Risk: callgraph scope expands into unfinished standing execution.** Integrate only at the existing builder seam unless the parent branch already supplies the runner.

## Verification

```bash
cargo test -p aft memory_admission
cargo test -p aft config_resolve
cargo test -p aft standing
cargo test -p aft search_staging
cargo test -p aft semantic_staging
cargo test -p aft callgraph_staging
cargo test -p aft health
bun test packages/opencode-plugin/src/__tests__/config.test.ts packages/pi-plugin/src/__tests__/config.test.ts
cargo fmt --all -- --check
cargo test -p aft --lib
AFT_GATE_PHASES=storm ./scripts/release-gate.sh
```

The exact release gate command must be reconciled with the current repository script before execution. The final verification report must list any pre-existing baseline failures separately from this change.
