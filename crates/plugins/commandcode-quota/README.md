# respan-gateway-plugin-commandcode-quota

`CommandCodeQuota` keeps a small cached view of each Command Code account's
plan windows and places unavailable accounts last in a route. It polls the
same read-only endpoints as `cmd /usage`; a provider call that receives a 429
leaves the rotation immediately.

```toml
[plugins.commandcode_quota]
poll_interval = "60s"
providers = [
  { name = "cc1", key = "env:CC1_KEY" },
  { name = "cc2", key = "env:CC2_KEY" },
  { name = "cc3", key = "env:CC3_KEY" },
]

[routes."deepseek-v4.1-flash"]
to = ["cc1/deepseek/deepseek-v4.1-flash", "cc2/deepseek/deepseek-v4.1-flash",
      "cc3/deepseek/deepseek-v4.1-flash", "direct/deepseek-flash"]
fallback = true
```

Available Command Code candidates are rotated per request. A route's other
providers remain after them as fallback. An account with a spent rolling
window remains available while purchased credits can bypass that window.

The plugin fails open when quota polling itself is unavailable: the configured
route keeps its normal behavior. A real provider 429 is handled immediately.
Quota endpoints have an internal `/alpha` prefix in Command Code CLI `1.88.5`,
so the plugin checks their contract at every poll and logs failures without
stopping the gateway.
