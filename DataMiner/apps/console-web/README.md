# @dataminer/console-web — archived source

This component is part of the Diogenes source archive. The original hosted service and deployment
automation are not included. Provision and configure your own local dependencies before running it.

From `DataMiner/`, install the locked workspace dependencies and use the available checks:

```bash
pnpm install --frozen-lockfile
pnpm --filter @dataminer/console-web typecheck
pnpm --filter @dataminer/console-web build
```

The implementation and tests define its current contracts. Example environment files, when present,
contain sample values only. Do not reuse historical production names or credentials.

No automatic deployment is configured. Worker configurations in this snapshot are local/examples:
no account IDs, custom-domain routes, cron triggers, or remote deployment scripts are included.
The mirror's D1 database ID must be supplied by the operator for any future deployment.

Consent text files are redacted historical templates. They are not a substitute for reviewed
terms for a new deployment and cannot certify any prior consent records.
