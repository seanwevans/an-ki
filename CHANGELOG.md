# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Real neural network training.** A multi-layer perceptron (`tanh` hidden
  layer, softmax output, cross-entropy loss) with backpropagation verified
  against finite differences, replacing the placeholder computation that
  returned twice its input.
- **A training task and data-parallel sharding.** A deterministic,
  non-linearly-separable dataset generated from a seed on every node, split into
  disjoint shards so each worker contributes independent gradient signal.
- Configurable `hidden_units`, `learning_rate`, `dataset_samples`,
  `dataset_seed`, and `init_seed`.
- **A held-out validation split.** `validation_fraction` (default 0.2) reserves
  part of the dataset for evaluation. Shards are cut from the training portion
  only, so no worker computes a gradient on a validation sample, and the An node
  reports accuracy on unseen data after each epoch.
- **Model checkpointing.** The An node writes parameters to the
  `model_checkpoints` table every `checkpoint_interval_epochs` and resumes from
  the most recent checkpoint on startup, so a restart no longer discards the
  training run. Checkpoints are encrypted at rest and keyed by a fingerprint of
  the network shape and dataset, so a configuration change starts a fresh run
  rather than resuming into a mismatched parameter vector.

### Fixed

- Opening the Raft store now retries briefly on I/O failure. sled holds an
  exclusive directory lock that is released when the previous handle drops, so a
  container restarting promptly could race the departing process and fail to
  start over a lock that was about to be released. Corruption and unsupported
  formats still fail immediately.

### Changed

- Ki nodes return a `GradientReply` carrying the mean gradient, the loss, and
  the sample count; An nodes combine shards weighted by sample count.
- Initial parameters are drawn from a seeded Xavier distribution rather than
  starting at zero, without which every hidden unit stays symmetric and the
  network cannot learn.
- Default `training_epochs` 10 → 400 and `learning_rate` 0.5 → 1.0: the previous
  values did not converge.

### Removed

- `model_dimension`, superseded by `hidden_units` — the parameter count now
  follows from the network shape.
- The `ki_model_queue` parameter broadcast. An nodes encrypted and published
  updated parameters every epoch, Ki nodes decrypted them into a field nothing
  read, and gradient requests already carry the parameters they are evaluated
  at. Removing it also removes a 100,000-iteration key derivation from a path
  that ran on every epoch.
- `messaging::encrypt_payload`, `decrypt_payload`, and `publish_encrypted`,
  whose only caller was that broadcast. Checkpoint encryption uses the
  `security` primitives directly.

## [0.1.0] - 2026-08-14

First tagged release. The system runs a complete training round across
principal, An, and Ki nodes, with consensus, discovery, and health monitoring
backed by working implementations rather than placeholders.

### Added

- **Training rounds that complete.** An nodes dispatch one task per model shard
  each epoch onto `ki_task_queue`; Ki nodes compute and reply on
  `an_task_queue`; the An node averages a full set of gradients and broadcasts
  the updated model on `ki_model_queue`. Configured by `model_shards`,
  `model_dimension`, `training_epochs`, and `epoch_interval_ms`.
- **Durable Raft consensus.** A `sled`-backed store holds the log, vote, and
  state machine, flushing the writes Raft cannot afford to lose before each call
  returns. A restarted principal resumes its cluster instead of re-initializing
  as a new one.
- **Multi-principal clusters.** Principals exchange Raft RPCs over HTTP
  (`/raft/append-entries`, `/raft/vote`, `/raft/install-snapshot`), with
  membership described by `RAFT_PEERS`. A cluster now tolerates the loss of a
  minority of its principals.
- **Replicated cluster state.** Role assignments and configuration overrides are
  `ClusterRequest` entries in the Raft log, requested over
  `principal_update_queue` and readable from any principal.
- **Heartbeat-derived node discovery.** The principal maintains a live
  `NodeRegistry` from heartbeats, evicting nodes silent for `NODE_TTL_MS` and
  logging cluster composition every `CLUSTER_REPORT_INTERVAL_MS`.
- **REST task API** on An nodes, authenticated with JWTs and role-checked, over
  a database-backed task recovery manager.
- **Deployment**: Helm chart running the principal as a `StatefulSet` with
  per-replica persistent volumes and ordinal-derived Raft node ids, plus a
  headless service for peer DNS.

### Changed

- `consensus_state` is published only by principals and driven by real Raft
  leadership, rather than being assumed from the command-line argument.
- `tasks_processed_total` and `task_processing_seconds` are now recorded on every
  successfully processed task, so the Grafana throughput panel reflects the
  cluster.
- Heartbeats carry the sender's role, letting the principal build a complete
  cluster view without a registration handshake.
- `NodeInfo::id` is a string, matching the identity `NODE_ID` actually carries.
- CI runs on every pull request regardless of base branch, and lints
  `--all-targets`.

### Removed

- `election`, superseded by openraft. Its `run_leader_election` never counted
  votes.
- `backup`, superseded by the database-backed task recovery manager.
- `validation`, unreferenced, and its message check rejected every JSON payload
  the nodes exchange.
- `dht`, an in-process `HashMap` that could not perform discovery; replaced by
  the heartbeat-derived registry.
- `load_balancer`, which selected a node id nothing could route to. Every Ki node
  consumes from the same queue, so the broker performs the distribution.
- The `database` cluster-update variant, which would have executed arbitrary SQL
  submitted by anyone able to publish to the queue.

### Known limitations

- The per-shard computation is a placeholder: `perform_computation` returns twice
  its input rather than a real gradient. This release provides the distributed
  round trip, not a training algorithm. (Addressed in Unreleased.)
- Tests behind the `integration-tests` feature, which exercise the live RabbitMQ
  and PostgreSQL paths, are not run in CI.
- The message-encryption key is derived from `jwt_secret_key`, so one secret
  covers both authentication and message confidentiality.

[Unreleased]: https://github.com/seanwevans/an-ki/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/seanwevans/an-ki/releases/tag/v0.1.0
