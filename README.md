# carrier-1nce

The 1NCE carrier package for Pulzr: an organization adds it from the console's
carrier catalog, types in its 1NCE API user, and its SIMs are managed through
it. It is a connectivity driver built as a WebAssembly module, carrier ABI 1
(pulzar-net/connectivity-sdk, `ABI.md`), run by the platform's connectivity
service in a sandbox.

The module never holds a credential and makes no request of its own. It asks
the host to call `api.1nce.com` (the one host `carrier.json` declares) and
writes `{{basic:client_id:client_secret}}` where the API user goes; the host
fills it in on the way out. Its OAuth token goes in the memo the host keeps
for the account, and 1NCE's records (a SIM's address, its quota, the SIM list)
are cached by the host for the times in `src/onence.rs`.

| | |
|---|---|
| `carrier.json` | the manifest: name, logos, capabilities, hosts, the credential form and the provider's configuration |
| `src/lib.rs` | the ABI: `pz_abi`, `pz_alloc`, `pz_call`, and `pz.http` behind a `Transport` |
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
