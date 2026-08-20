# dig-sex — Store EXchange

The **centralized policy layer** for exchanging DIG stores between peers.

It answers *which* store to exchange, *with whom*, *in what order*, and *why*. Discovery, transport
and storage already exist and stay where they are — `dig-dht`, `dig-pex`, `dig-download`,
`dig-store-cache`. This crate composes them behind one decision surface.

## Why centralize

Store exchange already happens, spread across several crates and dig-node's own subscription and
sync paths, with relevance decided differently in each. One decision surface means one place to
reason about, one place to change, and one place to swap the algorithm.

## Pluggable algorithms first

The exchange strategy is an interface. At least one future implementation is **incentivised**
exchange — peers trading on terms rather than relevance alone. The seam comes first; the first
algorithm is just the first implementation of it.

## Status

Scaffolding. The design is being measured against the exchange logic that exists today.

## License

MIT OR Apache-2.0.
