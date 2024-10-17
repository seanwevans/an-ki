# Distributed Neural Network System

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
- [Modules Overview](#modules-overview)
- [Development](#development)
  - [Testing](#testing)
  - [Building](#building)
- [Contributing](#contributing)
- [License](#license)

## Features

- **Task Scheduling:** Efficient task assignment using a load balancer and asynchronous execution.
- **Fault Tolerance:** Built-in backup and recovery mechanisms for task persistence.
- **Secure Communication:** JWT-based authentication and role-based access control.
- **Dynamic Node Discovery:** Uses a distributed hash table (DHT) for node management.
- **Leader Election:** Ensures high availability with automatic leader selection.
- **Monitoring and Metrics:** Supports Prometheus metrics and detailed logging for monitoring.
- **Configurable:** Easily configurable using environment variables and configuration files.

## Architecture

The system is composed of three main types of nodes:
1. **Principal Node:** Acts as the coordinator, responsible for role assignments and maintaining global state consistency.
2. **An Nodes:** Handle task distribution to Ki nodes and manage communication with the principal.
3. **Ki Nodes:** Execute tasks assigned by An nodes and report results back.

Inter-node communication is facilitated via RabbitMQ, and tasks are scheduled using a load balancer to optimize resource utilization.

![Architecture Diagram](architecture_diagram.png)

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
bash
Copy code
brew install cockroachdb/tap/cockroach
On Ubuntu/Debian:
bash
Copy code
sudo apt-get install -y cockroachdb
On Windows: Follow the official installation guide.
Start a single-node CockroachDB cluster (for local development):

```bash
cockroach start-single-node --insecure --listen-addr=localhost
```

4. **Configure the project:**
   Create a `config/` directory and add configuration files. You can start by copying the example configuration:
   ```bash
   mkdir -p config
   cp config/default.example config/default.toml
   ```
   Set up your environment variables or update the configuration files for your local setup.

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

Usage

Running the Nodes

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

API Endpoints

The system provides a REST API for managing tasks, available via the Warp web server. Below are the available endpoints:

GET /tasks: Retrieve a list of all tasks in the system.

curl http://localhost:3030/tasks

POST /tasks: Add a new task to the system. Provide the task data in the request body.

curl -X POST http://localhost:3030/tasks -H "Content-Type: application/json" -d '{"task_data": "sample task data"}'

DELETE /tasks/{task_id}: Delete a specific task by providing the task ID.

curl -X DELETE http://localhost:3030/tasks/{task_id}

These endpoints allow external interaction with the distributed network, such as adding new tasks or querying the current tasks.