# carrier-1nce

The 1NCE carrier driver for Pulzr: an organization adds it from the console's
carrier catalog, types in its 1NCE API user, and its SIMs are managed through
it. It is an extension module of the carrier kind (pulzar-net/connectivity-sdk,
`CARRIER.md`) on extension ABI 2 (pulzar-net/pulzr-protocol, `EXTENSIONS.md`),
written with `pulzr-package` and run by the platform's connectivity service in
its sandbox.

The module never holds a credential and makes no request of its own. It asks
the host to call `api.1nce.com` (the one host `carrier.json` declares) and
writes `{{basic:client_id:client_secret}}` where the API user goes; the host
fills it in on the way out. Its OAuth token goes in the install's `pz.state`
(one install per provider, so two 1NCE accounts never share one), and 1NCE's records (a SIM's address, its quota, the SIM list)
are cached by the host for the times in `src/onence.rs`.

| | |
|---|---|
| `carrier.json` | the manifest: name, logos, capabilities and the provider's configuration, and the module (its hosts, limits and credential form) |
| `src/lib.rs` | operations in, answers out; `pz.http` and `pz.state` behind a `Host` (the SDK's in the module, a recording in tests) |
| `src/onence.rs` | 1NCE: URLs, field names, quirks |
| `tests/replay.rs` | the driver against recorded, invented 1NCE responses |

Capabilities: `sim.inventory`, `sim.status`, `sim.usage`, `sim.suspend`,
`sim.activate`, `sim.topup`, `sim.reset`, `sim.lock`, `sim.location`,
`apn.profile`. `sim.sms` and the network capabilities are 1NCE's too and are
not implemented; a package claims only what it does. A top-up is a real order:
the platform decides whether one may be placed at all, and the provider's
configuration names the payment method.

```
cargo test
cargo build --release --target wasm32-unknown-unknown
```

A tag `vX.Y.Z` releases `carrier.json` (with the module's sha256 filled in),
`carrier.wasm` and the logos; the platform's catalog adds a release by its tag.

The 1NCE name and logos are 1NCE GmbH's, used to identify the service this
package connects to.

Apache-2.0.
