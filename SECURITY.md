# Security Policy

This document describes how to report vulnerabilities in AN-KI, how maintainers
triage reports, and the security controls expected when developing, deploying,
and operating the distributed neural network system.

## Supported Versions

AN-KI is currently pre-1.0 software. Security fixes are applied to the active
`main` development line unless a release branch is explicitly announced by the
maintainers.

| Version / branch | Security support |
| --- | --- |
| `main` | Supported |
| Tagged pre-1.0 releases | Best-effort support until superseded |
| Forks, snapshots, and unmaintained branches | Not supported by this project |

If you operate a fork or a long-lived deployment branch, you are responsible for
backporting fixes or regularly rebasing onto the supported branch.

## Reporting a Vulnerability

Please report suspected vulnerabilities privately first. Do not open a public
issue, discussion, or pull request that includes exploit details, secrets,
proof-of-concept payloads, credentials, private keys, or sensitive operational
logs.

Preferred reporting channels, in order:

1. Use GitHub's private vulnerability reporting feature if it is enabled for the
   repository.
2. Contact the repository maintainers through the private security contact listed
   in the repository metadata or organization profile.
3. If no private contact is available, open a minimal public issue asking for a
   private security contact, but do not include technical details.

Include the following information when possible:

- Affected component, node type, endpoint, crate, container image, Helm chart, or
  deployment mode.
- Exact commit, tag, image digest, configuration profile, and relevant feature
  flags.
- Step-by-step reproduction details or a minimal proof of concept.
- Expected and observed behavior.
- Impact assessment, including confidentiality, integrity, availability, or
  privilege-escalation consequences.
- Logs, traces, packet captures, or screenshots with secrets redacted.
- Whether the vulnerability is actively exploited or publicly known.

## Disclosure and Response Process

Maintainers should follow this process for incoming reports:

1. **Acknowledge receipt** as soon as practical, ideally within 3 business days.
2. **Triage severity** using the [Severity Guidelines](#severity-guidelines).
3. **Reproduce and scope** the issue across supported branches, node roles, API
   routes, deployment manifests, and documented configurations.
4. **Develop a fix privately** when exploitability or operational risk warrants
   embargo.
5. **Validate the fix** with targeted regression tests and relevant integration,
   container, or Helm checks.
6. **Release and communicate** the fix with clear upgrade guidance, mitigation
   steps, and credit when requested by the reporter.
7. **Publish details** only after users have had a reasonable opportunity to
   patch, unless the issue is already public or actively exploited.

Safe-harbor intent: good-faith security research that avoids privacy violations,
data destruction, service disruption, persistence, lateral movement, and access
to third-party systems is welcome. Stop testing and report immediately if you
encounter sensitive data or discover a path that could impact real users or
infrastructure.

## Severity Guidelines

Use the following rubric as a starting point. Final severity depends on network
exposure, exploit prerequisites, deployment configuration, and compensating
controls.

| Severity | Examples |
| --- | --- |
| Critical | Remote code execution; unauthenticated administrative access; compromise of JWT signing secrets, CA private keys, database credentials, or model checkpoints; exploit chains that let an attacker control principal, An, or Ki nodes. |
| High | Authentication or authorization bypass; message forgery; TLS validation bypass; privilege escalation between node roles; unauthorized task creation/deletion in exposed deployments; persistent denial of service against cluster coordination. |
| Medium | Sensitive information disclosure in logs or metrics; weak default configuration likely to reach production; replay or downgrade risk requiring network position; resource exhaustion requiring authentication or local access. |
| Low | Hardening gaps, missing documentation, non-sensitive error disclosure, low-impact dependency advisories, or issues requiring unrealistic prerequisites. |

## Security Architecture Overview

AN-KI consists of Principal, An, and Ki nodes. The system coordinates tasks,
persists recovery data, communicates through RabbitMQ, exposes task-management
APIs, and can run under Kubernetes with Helm.

Primary security boundaries:

- **Node identity and role boundaries:** Principal, An, and Ki nodes must only
  receive permissions required for their role.
- **Inter-node transport:** Node-to-node communication should use authenticated
  TLS with certificates issued by a trusted project CA.
- **Message broker boundary:** RabbitMQ credentials, vhosts, exchanges, and
  queues must be isolated per environment.
- **Database boundary:** CockroachDB/PostgreSQL credentials must be least
  privilege and protected in transit.
- **Operator/API boundary:** REST API exposure must be intentionally restricted,
  authenticated by an external gateway when reachable outside a trusted network,
  and monitored.
- **Observability boundary:** Logs, Prometheus metrics, traces, and dashboards
  must not leak secrets or sensitive payloads.

## Cryptography and Identity Requirements

### JWTs

- Generate `JWT_SECRET_KEY` with a cryptographically secure random generator.
  Use at least 256 bits of entropy for HMAC-based signing secrets.
- Prefer `JWT_SECRET_KEY_FILE` or an orchestrator-managed secret file over
  inline environment variables when supported by your deployment platform.
- Never use placeholder values such as `change-me`, `<JWT_SECRET_KEY>`, sample
  secrets, or values committed to source control.
- Rotate JWT signing secrets after suspected disclosure, staff transitions,
  environment compromise, or policy-defined rotation intervals.
- Use short token lifetimes for node bootstrap flows and renew tokens through
  trusted control-plane paths only.
- Treat JWTs as bearer credentials. Redact them from logs, traces, metrics,
  crash reports, support bundles, and shell history.

### Message Encryption

- Application-level encrypted messages should use high-entropy keys that are
  distinct from JWT signing secrets, database passwords, RabbitMQ passwords, and
  TLS private keys.
- Use a separate encryption key per environment. Production, staging,
  development, and test environments must not share keys.
- Rotate encryption keys using a planned migration procedure so in-flight and
  stored encrypted payloads remain recoverable as needed.

### TLS and Certificates

- Use a dedicated private CA for AN-KI node identity. Protect the CA private key
  offline or in a hardened secrets manager.
- Issue unique certificates for each node identity and include the expected DNS
  name or service name in the certificate subject alternative names.
- Enable mutual TLS for inter-node traffic whenever nodes communicate across a
  network boundary.
- Do not disable TLS verification in production. Any test-only insecure TLS
  settings must remain isolated to tests and local development.
- Rotate node certificates before expiration and immediately after suspected
  private-key compromise.
- Restrict certificate and private-key file permissions to the process owner
  wherever possible.

## Configuration and Secrets Management

Required production practices:

- Store secrets in a managed secret store, Kubernetes `Secret`, sealed-secret
  workflow, external secrets operator, or equivalent platform facility.
- Keep `config/default.example` as documentation only. Do not copy example
  placeholder values into production.
- Ensure local `config/default.toml` files containing secrets are ignored by Git
  and protected by workstation controls.
- Use separate RabbitMQ, database, JWT, encryption, and TLS credentials per
  environment.
- Disable shell command echoing and avoid writing secrets into terminal history
  when exporting environment variables.
- Audit CI logs, container build logs, and Helm release histories for accidental
  secret exposure.
- Prefer immutable image digests and externally supplied runtime secrets over
  baking secrets into container images.

Minimum Kubernetes expectations:

- Mount secrets as files where practical and mark volumes read-only.
- Set `runAsNonRoot`, drop Linux capabilities, use a read-only root filesystem
  when compatible, and define resource requests and limits.
- Apply network policies that only allow required node, broker, database,
  metrics, and operator traffic.
- Do not expose internal services through public `LoadBalancer`, `NodePort`, or
  ingress resources without authentication, authorization, TLS, and rate limits.
- Review rendered manifests before applying Helm values to production.

## API Security

The REST task API can create, read, and delete tasks. Treat it as a privileged
control surface.

Production deployments should place the API behind controls such as:

- A mutually authenticated service mesh, API gateway, or reverse proxy.
- Strong authentication for operators or service callers.
- Authorization policies that restrict task creation, retrieval, and deletion by
  identity and environment.
- TLS termination with modern protocol and cipher settings.
- Request-size limits, body parsing limits, rate limits, and timeout policies.
- Audit logging for task mutations, with sensitive payloads redacted.
- CORS restrictions if browser-based access is introduced.

Do not directly expose the API to the public internet unless these controls are
in place and explicitly reviewed.

## RabbitMQ Security

- Use authenticated RabbitMQ users; do not rely on default guest credentials in
  shared or production environments.
- Use one vhost per environment and grant only the queue/exchange permissions
  required by each service account.
- Prefer AMQPS or broker-side TLS for networked deployments.
- Configure queue durability, message TTLs, dead-lettering, maximum queue sizes,
  and consumer prefetch limits according to the deployment's availability and
  denial-of-service requirements.
- Monitor connection churn, authentication failures, unroutable messages, queue
  depth, and consumer lag.
- Rotate broker credentials and revoke unused users promptly.

## Database Security

- Use TLS for database connections outside local-only development.
- Use least-privilege database users instead of administrative accounts for
  application runtime.
- Restrict database network access to application nodes and administrative
  bastions.
- Encrypt database storage using platform or cloud-provider controls.
- Back up data regularly, test restore procedures, and protect backup media with
  equivalent or stronger controls than the primary database.
- Avoid storing secrets, credentials, tokens, or private keys in task payloads,
  model metadata, or checkpoints.

## Observability, Logs, and Metrics

- Redact or hash secrets, JWTs, certificates, keys, database URLs, AMQP URLs,
  task payloads that may contain sensitive data, and personally identifiable
  information before logging.
- Treat traces and metrics as potentially sensitive because labels and spans can
  reveal topology, node IDs, queue names, database names, task IDs, or workload
  characteristics.
- Restrict access to Prometheus, OpenTelemetry Collector endpoints, Grafana, and
  raw log stores.
- Use retention periods appropriate for the sensitivity of operational data.
- Alert on authentication failures, token validation errors, TLS failures,
  unexpected leader changes, sustained queue backlogs, anomalous task deletion,
  and repeated node reconnects.

## Supply Chain Security

- Review Rust dependency updates for security advisories and breaking changes.
- Run dependency and container image vulnerability scans in CI before release.
- Build release images from clean, pinned, reproducible inputs when possible.
- Pin GitHub Actions, CI images, Helm dependencies, and base images to trusted
  versions or immutable digests.
- Do not install unreviewed build tools, scripts, or binaries in release
  pipelines.
- Generate SBOMs for release artifacts when possible and retain provenance for
  images and binaries.

Recommended local checks before merging security-sensitive changes:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo audit
cargo deny check
```

`cargo audit` and `cargo deny` require additional developer tooling and policy
configuration; use equivalent scanners if your environment standardizes on other
tools.

## Secure Development Guidelines

- Validate all external input at trust boundaries, including API bodies, task
  identifiers, queue names, node addresses, certificates, and configuration.
- Prefer allowlists over blocklists for identifiers, roles, and protocol choices.
- Use structured errors that are useful to operators but avoid returning
  sensitive internal details to clients.
- Keep cryptographic operations centralized and covered by tests.
- Avoid logging full URLs when they may contain credentials.
- Treat model checkpoints and training data as potentially sensitive artifacts.
- Add regression tests for every fixed vulnerability.
- Document threat-model changes when adding new ports, node roles, queues,
  metrics, secrets, storage paths, or third-party services.
- Run security-focused code review for changes touching authentication,
  authorization, cryptography, serialization, network listeners, database access,
  container build steps, or Helm manifests.

## Deployment Hardening Checklist

Before production deployment, verify that:

- [ ] All placeholder secrets have been replaced with high-entropy values.
- [ ] JWT, encryption, RabbitMQ, database, and TLS credentials are unique per
      environment.
- [ ] Inter-node traffic uses authenticated TLS and trusted node certificates.
- [ ] RabbitMQ uses non-default users, least-privilege vhost permissions, and TLS
      when traffic leaves a local trust boundary.
- [ ] Database access uses least privilege and TLS outside local development.
- [ ] REST APIs are not publicly exposed without gateway authentication,
      authorization, rate limits, and TLS.
- [ ] Metrics, traces, logs, and Grafana dashboards are access controlled.
- [ ] Network policies or firewall rules restrict node, broker, database, and
      observability traffic.
- [ ] Containers run as non-root where possible and have resource limits.
- [ ] Backups are encrypted, access controlled, and regularly restore-tested.
- [ ] Dependency, image, and manifest scans have been reviewed.
- [ ] Incident contacts, escalation paths, and rollback procedures are current.

## Incident Response

If compromise is suspected:

1. Preserve evidence by snapshotting relevant logs, traces, metrics, images,
   manifests, and database state without exposing secrets further.
2. Contain affected nodes, revoke network access, or scale down workloads as
   needed to stop ongoing abuse.
3. Rotate potentially exposed JWT secrets, encryption keys, RabbitMQ users,
   database credentials, TLS node certificates, and CA material if necessary.
4. Rebuild and redeploy from trusted source and clean images.
5. Verify data integrity for tasks, checkpoints, model artifacts, database
   records, queues, and backups.
6. Review audit logs for unauthorized task creation, deletion, replay,
   privilege escalation, lateral movement, and data exfiltration.
7. Notify affected parties according to legal, contractual, and project
   obligations.
8. Add regression tests, hardening changes, and documentation updates before
   closing the incident.

## Security Non-Goals and Assumptions

- AN-KI does not replace an API gateway, identity provider, service mesh,
  Kubernetes policy engine, secrets manager, or SIEM.
- Local development examples may use insecure settings for convenience. These
  examples are not production guidance.
- Operators are responsible for securing the host, orchestrator, container
  runtime, network, database, message broker, and observability stack.
- Training data, model checkpoints, and task payloads may require additional
  privacy, compliance, or data-governance controls outside the scope of this
  repository.
