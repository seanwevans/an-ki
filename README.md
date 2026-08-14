# Distributed Neural Network System
<img width="256" alt="Neural Network Diagram on Teal Background" src="https://github.com/user-attachments/assets/063a74c0-0aed-4905-a72d-8236bdfd65b7" />

A distributed neural network project that supports task scheduling, load balancing, fault tolerance, and secure communication across a network of nodes. This system is designed for high availability and scalability, with asynchronous operations, health monitoring, and leader election to ensure robustness.

## Table of Contents
- [Features](#features)
- [Architecture](#architecture)
- [Getting Started](#getting-started)
  - [Prerequisites](#prerequisites)
  - [Installation](#installation)
  - [Configuration](#configuration)
- [Usage](#usage)
  - [Running the Nodes](#running-the-nodes)
  - [API Endpoints](#api-endpoints)
- [Deployment](#deployment)
- [Modules Overview](#modules-overview)
- [Development](#development)
  - [Testing](#testing)
  - [Building](#building)
- [Contributing](#contributing)
- [License](#license)

## Features

- **Task Scheduling:** Efficient task assignment using a load balancer and asynchronous execution.
- **Fault Tolerance:** Tasks are persisted to the database by the task recovery manager, so they survive a node restart and can be recovered by ID.
- **Secure Communication:** JWT-based authentication and role-based access control on the REST API, plus AES-256-GCM encryption of model-update messages exchanged between nodes (keyed by `jwt_secret_key`).
- **Dynamic Node Discovery:** The principal derives a live cluster view from node heartbeats — nodes join by heartbeating and are evicted after `NODE_TTL_MS` of silence. No separate registration step is required.
- **Consensus & Leader Election:** Uses the Raft protocol (via [`openraft`](https://github.com/datafuselabs/openraft)) for a replicated, consistent log and automatic leader election. The principal runs a Raft node — a single-member cluster today, with multi-node operation arriving once the networking transport in `raft_node` is implemented.
- **Monitoring and Metrics:** Supports Prometheus metrics and detailed logging for monitoring.
- **Configurable:** Easily configurable using environment variables and configuration files.

### Task Recovery Consistency

Tasks are first inserted into an in-memory map and then written to the database.
If the database write fails, the in-memory insert is rolled back and the error is
surfaced to the caller. Clients should only rely on a task being present after
handling the `Result` from the add operation.

## Architecture

The system is composed of three main types of nodes:
1. **Principal Node:** Acts as the coordinator, responsible for role assignments and maintaining global state consistency.
2. **An Nodes:** Handle task distribution to Ki nodes and manage communication with the principal.
3. **Ki Nodes:** Execute tasks assigned by An nodes and report results back.

Inter-node communication is facilitated via RabbitMQ, and tasks are scheduled using a load balancer to optimize resource utilization.

### Health Monitoring and Node Discovery

An and Ki nodes publish periodic heartbeats to the `heartbeat_queue` (every
`HEARTBEAT_INTERVAL_MS`, default 10s). Each heartbeat carries the sender's
identity and role. Each node identifies itself with the `NODE_ID` environment
variable when set, or a generated UUID otherwise.

The principal consumes these heartbeats and uses them for two things:

1. **Health tracking.** It logs a corrective-action alert once a node reports
   `HEALTH_UNHEALTHY_THRESHOLD` consecutive unhealthy checks (default 3).
2. **Cluster membership.** It maintains a `NodeRegistry` — the live set of nodes
   and their roles. A node joins the registry the first time it heartbeats and
   is evicted once it has been silent for `NODE_TTL_MS` (default 30s, which
   tolerates two missed beats at the default interval). The principal logs the
   cluster composition every `CLUSTER_REPORT_INTERVAL_MS` (default 60s).

Membership is derived entirely from heartbeats, so there is no registration
handshake to get wrong: a restarted node reappears under its own identity, and a
node that dies disappears on its own.

| Variable | Default | Purpose |
| --- | --- | --- |
| `NODE_ID` | generated UUID | Identity a node heartbeats under |
| `HEARTBEAT_INTERVAL_MS` | `10000` | How often a node heartbeats |
| `HEALTH_UNHEALTHY_THRESHOLD` | `3` | Consecutive bad reports before alerting |
| `NODE_TTL_MS` | `30000` | Silence before a node is evicted from the registry |
| `CLUSTER_REPORT_INTERVAL_MS` | `60000` | How often the principal logs the cluster view |


### Replicated Cluster State

The principal's authoritative state — which role each node is assigned, and
cluster-wide configuration overrides — lives in the Raft log rather than in one
process's memory. Every change is a `ClusterRequest` entry
(`AssignRole`, `ClearRole`, `SetConfig`) that is committed through Raft and then
folded into the state machine, so each principal derives the same state from the
same log.

The log, the state machine, and the vote are persisted to a local `sled`
database under `RAFT_DATA_DIR` (default `data/raft`, with a per-node
subdirectory named after `RAFT_NODE_ID`). Writes Raft cannot afford to lose —
the vote and appended entries — are flushed before the storage call returns. A
principal that restarts reloads its log and resumes the existing cluster instead
of re-initializing as a fresh one.

### Cluster Update Requests

Cluster-wide changes are requested by publishing to `principal_update_queue`.
The principal validates each request, commits it to the Raft log, and only then
acknowledges the message — so an accepted request is replicated to every
principal, not applied in one process's memory.

```json
{"update_id": "1", "content": {"type": "assign_role", "data": {"node_id": "ki-0", "role": "Ki"}}}
{"update_id": "2", "content": {"type": "clear_role",  "data": {"node_id": "ki-0"}}}
{"update_id": "3", "content": {"type": "set_config",  "data": {"key": "model_shards", "value": "4"}}}
```

How a request is settled depends on why it failed:

- **Applied** — committed through Raft; the message is acknowledged.
- **Rejected** — the request can never succeed (malformed payload, blank
  identifier). It is dead-lettered rather than requeued, since retrying a
  message that can only fail again just burns the queue.
- **Requeued** — the request is valid but this principal is not the Raft leader,
  or its Raft node is not running. The message goes back on the queue for the
  leader to pick up.

> **Note:** there is deliberately no request type for executing SQL. An earlier
> `database` variant accepted an arbitrary statement from anyone able to publish
> to the queue. Schema changes belong in `migrations/`; data changes belong
> behind the authenticated task API.

### Forming a Multi-Principal Cluster

Principals carry Raft RPCs to each other over HTTP. Each one serves
`/raft/append-entries`, `/raft/vote` and `/raft/install-snapshot` on `RAFT_ADDR`
and dials its peers at the addresses recorded in the cluster membership.

The cluster is described by `RAFT_PEERS`, a comma-separated list of
`id=host:port` entries naming **every** principal, including this one:

```bash
export RAFT_NODE_ID=1
export RAFT_PEERS="1=principal-0.principal-headless:4001,2=principal-1.principal-headless:4001,3=principal-2.principal-headless:4001"
```

The lowest-numbered peer initializes the cluster; the others start empty and
receive the membership through replication. Leaving `RAFT_PEERS` unset runs a
single-member cluster, which is the local-development default. A principal whose
`RAFT_NODE_ID` does not appear in `RAFT_PEERS` refuses to start rather than
quietly forming a cluster of its own.

Use an odd number of principals: Raft needs a majority to commit, so 3 members
tolerate 1 failure and 5 tolerate 2, while 2 members tolerate none.

| Variable | Default | Purpose |
| --- | --- | --- |
| `RAFT_NODE_ID` | `1` | This principal's numeric Raft id; must be unique per principal |
| `RAFT_DATA_DIR` | `data/raft` | Directory holding the Raft log and state machine |
| `RAFT_ADDR` | `0.0.0.0:4001` | Address this principal serves Raft RPCs on |
| `RAFT_PEERS` | unset | Full cluster membership; unset means single-member |
| `RAFT_RPC_TIMEOUT_MS` | `2000` | Per-RPC timeout before a peer is treated as unreachable |

Because this state is on disk, the Helm chart runs the principal as a
`StatefulSet` with a per-replica `PersistentVolumeClaim`, derives `RAFT_NODE_ID`
from the pod ordinal, and builds `RAFT_PEERS` from the replica count against the
headless service's per-pod DNS names. Running it as a `Deployment` on ephemeral
storage would hand a rescheduled principal an empty log.

## Getting Started

### Prerequisites

- **Rust** (latest stable version): Install from [rustup.rs](https://rustup.rs/)
- **RabbitMQ:** Install via [official RabbitMQ installation guide](https://www.rabbitmq.com/download.html)
- **Prometheus:** For metrics collection (optional)
- **CockroachDB:** Install via [official CockroachDB installation guide](https://www.cockroachlabs.com/docs/stable/install-cockroachdb.html) for distributed database management.


### Installation

1. **Clone the repository:**
   ```bash
   git clone https://github.com/your-username/distributed-neural-network.git
   cd distributed-neural-network
   ```

2. **Install Rust toolchain:**
   Make sure you have Rust installed. If not, install it using [rustup](https://rustup.rs/):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
   After installation, ensure Rust is accessible:
   ```bash
   rustc --version
   ```

3. **Set up RabbitMQ:**
   If RabbitMQ is not already installed, follow these steps:
   - **On macOS (using Homebrew):**
     ```bash
     brew update
     brew install rabbitmq
     ```
   - **On Ubuntu/Debian:**
     ```bash
     sudo apt-get update
     sudo apt-get install rabbitmq-server
     ```
   - **On Windows:** Follow the official [installation guide](https://www.rabbitmq.com/install-windows.html).

   Start the RabbitMQ server:
   ```bash
   rabbitmq-server
   ```

Set up CockroachDB: If CockroachDB is not already installed, follow these steps:

On macOS (using Homebrew):
brew install cockroachdb/tap/cockroach
On Ubuntu/Debian:
sudo apt-get install -y cockroachdb
On Windows: Follow the official installation guide.
Start a single-node CockroachDB cluster (for local development):

```bash
cockroach start-single-node --insecure --listen-addr=localhost
```

Set the connection string for the application. The project uses
`tokio-postgres` to connect to CockroachDB, so provide a standard
PostgreSQL URL:

```bash
export DATABASE_URL="postgresql://root@localhost:26257/defaultdb?sslmode=disable"
```

4. **Configure the project:**
   Create a `config/` directory and add configuration files. Start by copying the provided example configuration:
   ```bash
   mkdir -p config
   cp config/default.example config/default.toml
   ```
   Edit `config/default.toml` to set values for `amqp_addr`, `jwt_secret_key`, `database_url`,
   and optionally `otlp_endpoint` to point to your OpenTelemetry collector
   (default is `http://localhost:4317`). You can also supply the JWT secret via the
   `JWT_SECRET_KEY` environment variable and the OTLP endpoint via `OTLP_ENDPOINT` if preferred.

5. **Build the project:**
   Compile the project to ensure there are no issues.
   ```bash
   cargo build --release
   ```

6. **Run tests:**
   It's recommended to run the tests to verify that everything is set up correctly.
   ```bash
   cargo test
   ```

Now you're ready to start running the nodes as described in the [Usage](#usage) section.

## Usage

### Running the Nodes

To run the different nodes, follow the steps below. Each type of node should be run in a separate terminal session to simulate a distributed system.

Principal Node:
Run the principal node, which coordinates the overall system and assigns roles.

cargo run -- principal

An Node:
Run one or more An nodes to handle task distribution and communication.

cargo run -- an

Ki Node:
Run one or more Ki nodes to execute tasks assigned by An nodes and report results back.

cargo run -- ki

Make sure all nodes are running simultaneously to ensure proper communication and task assignment.

### API Endpoints

Each **An node** hosts this REST API (via the Warp web server) on the address
configured by `api_addr` (default `0.0.0.0:3030`, overridable with the `API_ADDR`
environment variable). The endpoints are backed by the database-backed task
recovery manager, so a reachable `database_url` is required. Below are the
available endpoints:

GET /tasks/{task_id}: Retrieve a specific task by providing its ID.

curl http://localhost:3030/tasks/{task_id}

POST /tasks: Add a new task to the system. Provide the task data in the request body.

curl -X POST http://localhost:3030/tasks -H "Content-Type: application/json" -d '{"task_data": "sample task data"}'

DELETE /tasks/{task_id}: Delete a specific task by providing the task ID.

curl -X DELETE http://localhost:3030/tasks/{task_id}

These endpoints allow external interaction with the distributed network, such as adding new tasks or querying the current tasks.

## Deployment

### Docker Images

Separate Dockerfiles are provided for each node type. Build the images using:

```bash
docker build -f Dockerfile.principal -t an-ki:principal .
docker build -f Dockerfile.an -t an-ki:an .
docker build -f Dockerfile.ki -t an-ki:ki .
```

Each image runs the appropriate node (`principal`, `an`, or `ki`) when started.

### Helm Chart

A Helm chart in `helm/an-ki` simplifies Kubernetes deployment. Replica counts and core
settings are configurable through `values.yaml`, with environment-specific overrides
available in files like `values-dev.yaml` and `values-prod.yaml`.

Deploy to a cluster with:

```bash
helm install an-ki ./helm/an-ki -f helm/an-ki/values.yaml -f helm/an-ki/values-dev.yaml
```

Override the `values-dev.yaml` file with `values-prod.yaml` or your own file for
production environments.

### Service Discovery

Within Kubernetes, each node is exposed via a `Service`, enabling discovery through
Kubernetes DNS (e.g., `principal.default.svc.cluster.local`). Outside Kubernetes,
nodes find each other through the broker: everything is addressed by queue name,
and the principal learns cluster membership from heartbeats (see
[Health Monitoring and Node Discovery](#health-monitoring-and-node-discovery)),
so only `amqp_addr` needs to be configured.

## Development

### Pre-commit Hooks

This project uses [pre-commit](https://pre-commit.com/) to automatically run `cargo fmt` and `cargo clippy` on commits. Install pre-commit and set up the hooks:

```bash
pip install pre-commit
pre-commit install
```

Run all checks manually with:

```bash
pre-commit run --all-files
```

## Monitoring

### OpenTelemetry Collector

An OpenTelemetry Collector can aggregate metrics and traces from regional nodes. A sample configuration is available at
`config/otel-collector-config.yaml`. Run the collector with Docker:

```bash
docker run --rm -p 4317:4317 -p 9464:9464 \
  -v $(pwd)/config/otel-collector-config.yaml:/etc/otel-collector-config.yaml \
  otel/opentelemetry-collector:latest \
  --config /etc/otel-collector-config.yaml
```

Nodes export traces to the collector via OTLP on port `4317` (configurable via the
`otlp_endpoint` setting or `OTLP_ENDPOINT` environment variable) and expose Prometheus metrics on port `9090`.
The collector scrapes these metrics and re-exports them on `9464` for federation.

### Grafana Dashboard

Import the sample dashboard found at `config/grafana/node_overview.json` into Grafana. It visualizes:

- **Node Status:** current health of each node.
- **Consensus State:** leader or follower role.
- **Task Throughput:** rate of tasks processed per minute.

Configure Grafana to use the collector's Prometheus exporter (`http://localhost:9464`) as a data source.
