# pulzr-connectivity-1nce

The 1NCE connector for [pulzr-connectivity](https://github.com/pulzar-net/connectivity-sdk).

Declares `sim.inventory`, `sim.status`, `sim.usage`, `sim.suspend`,
`sim.activate`, `sim.topup`, `sim.reset`, `sim.lock`, `sim.location` and
`apn.profile`. `sim.sms` and the network capabilities are 1NCE's too and are not
implemented yet; a connector claims only what it does.

```python
from pulzr_connectivity_1nce import OneNce

driver = OneNce()
driver.list_sims(creds, {})   # creds: namespace, client_id, client_secret
```

Top-ups place a real order, so they stay off unless `CELLULAR_TOPUPS_ENABLED=true`
and `ONENCE_TOPUP_PAYMENT_METHOD` is set. Tests replay recorded, scrubbed
responses and never reach 1NCE.

Apache-2.0.
