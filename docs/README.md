# The browser demo

A static page that runs the cluster: a principal quorum holding Raft elections, an
An node dispatching training rounds, Ki workers computing gradients over their own
shard, and a broker moving messages between them. It trains a real network to
about 99% accuracy on held-out data, and it keeps running when you start killing
nodes.

Live at **https://seanwevans.github.io/an-ki/** once Pages is enabled (see below).

```
docs/
├── index.html                  the page
├── assets/
│   ├── style.css
│   ├── app.js                  controls, animation loop, conformance check
│   ├── sim/
│   │   ├── rng.js              ChaCha12 + seed_from_u64 + gen_range, ported from rand 0.8
│   │   ├── dataset.js          port of src/dataset.rs
│   │   ├── model.js            port of src/model.rs
│   │   ├── broker.js           RabbitMQ dispatch, acknowledgement and requeue behaviour
│   │   ├── raft.js             elections, log matching, commitment by majority
│   │   ├── cluster.js          the nodes, the scheduler, heartbeats, checkpoints
│   │   └── fixtures/
│   │       └── reference.json  values printed by the Rust crate
│   └── ui/                     canvas and DOM rendering
└── tests/                      node --test suite over the simulation
```

## Running it locally

There is no build step and no dependencies. ES modules need a real origin, so
open it through a server rather than from the filesystem:

```bash
cd docs
python3 -m http.server 8000     # or: npx serve .
# http://localhost:8000
```

Run the tests with any Node 18 or newer:

```bash
cd docs
node --test tests/*.test.mjs
```

## Deploying it

The repository ships `.github/workflows/pages.yml`, which runs the test suite and
publishes `docs/` on every push to `main` that touches it. To turn it on, set
**Settings → Pages → Build and deployment → Source** to **GitHub Actions**. The
workflow needs no secrets.

If you would rather not use Actions, the same directory works as a branch
deployment: set the source to **Deploy from a branch**, `main`, folder `/docs`.
Every path in the page is relative, so it works from a project subpath without
configuration.

## How faithful is it?

Faithful where it can be checked, and explicit where it cannot.

**The same numbers.** `rng.js` reimplements what `StdRng` does in rand 0.8 —
`seed_from_u64`'s PCG32 expansion, ChaCha12, and `UniformFloat`'s `f32` sampling
— so `dataset::generate` and `MlpSpec::initialize` produce the same values in the
browser as they do in the cluster, bit for bit. `src/bin/demo_fixture.rs` prints
those values from the crate itself; `tests/rng.test.mjs` asserts the port
reproduces every one of them, and CI regenerates the fixture on each run so the
two cannot drift apart. The page performs the same check on load and says so in
the badge at the top right.

**The same arithmetic.** The forward and backward passes are a direct port,
computed in `f32`, and checked against central finite differences the way
`src/model.rs` checks its own. A test runs 40 epochs across four shards and 40
epochs on a single machine and asserts the parameter vectors agree: the An node's
sample-weighted average of per-shard mean gradients *is* the mean gradient over
the dataset, so sharding changes the message flow and not the model.

**The same failure behaviour.** Deliveries stay unacknowledged until a worker has
published its reply, so stopping one requeues its work for whoever is still
alive. Stopping all of them backs the queue up while the An node waits on a set
of gradients that will never be complete. Raft holds real elections, refuses to
commit without a majority, and reloads a restarted principal's log instead of
starting it empty. The An node restores its most recent checkpoint rather than
beginning again from `init_seed`.

**What is not real.** The broker is a model of RabbitMQ's behaviour, not
RabbitMQ; consensus is a compact Raft rather than `openraft` over `sled`. There
is no database, no JWT verification and no TLS — encryption at rest is reported
in the log, not performed. Time is scaled so a packet is visible: links carry
about 45ms and a shard takes tens of milliseconds, against a 400ms epoch, where
the shipped configuration dispatches every 100ms onto a local broker and closes a
round long before the next one starts. Dispatching faster than rounds close mixes
gradients taken at different parameter points; the page will warn you about it
rather than hide it, because the cluster has the same property.

## Changing it

If you change `src/dataset.rs` or `src/model.rs` in a way that moves the numbers,
regenerate the fixture in the same commit:

```bash
cargo run --bin demo_fixture > docs/assets/sim/fixtures/reference.json
```

CI fails otherwise, which is the point.
