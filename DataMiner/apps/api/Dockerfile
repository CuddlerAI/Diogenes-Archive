# syntax=docker/dockerfile:1
#
# Build @dataminer/api (a pnpm-workspace package) into a self-contained Cloud Run
# image. The build context MUST be the DataMiner repo root, because the pnpm
# lockfile and workspace manifest live there:
#
#   docker build -f apps/api/Dockerfile -t <image> .
#
# Cloud Build uses apps/api/cloudbuild.yaml, which sets the same context.
#
# One image, two entry points: the ingestion API (dist/index.js, the default CMD)
# and the maintenance worker (dist/worker.js — verification + purge + orphan
# sweep). Deploy the worker as a separate Cloud Run service/job that overrides the
# command; both share this image. The migration runner (dist/migrate.js) is a
# one-shot run with the same image.
#
# The service must run with HOST=0.0.0.0 (the app defaults to 127.0.0.1, which
# would refuse Cloud Run traffic) and honours the injected PORT (defaults to 8080).

FROM rust:1.88-bookworm AS catalog-validator-build
WORKDIR /repo
COPY . .
RUN cargo build --locked --release --manifest-path tools/catalog-validator/Cargo.toml

FROM node:22-bookworm-slim AS build
ENV PNPM_HOME=/pnpm PATH=/pnpm:$PATH CI=1
RUN corepack enable
WORKDIR /repo

# Manifests first so the dependency layer caches until a package.json changes.
# Every workspace member is copied because --frozen-lockfile validates the lock
# against all manifests, even when the install itself is filtered to one package.
# That means this list must gain a line whenever `apps/*` gains a member — a
# missing one fails the build here, not at runtime, which is the good direction
# but is easy to misread as a lockfile problem.
COPY pnpm-lock.yaml pnpm-workspace.yaml package.json ./
COPY apps/api/package.json apps/api/package.json
COPY apps/api-edge/package.json apps/api-edge/package.json
COPY apps/client/package.json apps/client/package.json
COPY apps/console-api/package.json apps/console-api/package.json
COPY apps/console-web/package.json apps/console-web/package.json
# console-api declares @dataminer/mirror (workspace:*); the --legacy deploy
# resolves the whole workspace graph, so this manifest must be present even
# though @dataminer/api never depends on it.
COPY apps/mirror/package.json apps/mirror/package.json

# Only @dataminer/api and its dependencies, production deps only in the bundle.
RUN pnpm install --frozen-lockfile --filter @dataminer/api...

COPY apps/api apps/api
# Build, then produce a self-contained deploy bundle with a real (non-symlinked)
# node_modules under /app.
RUN pnpm --filter @dataminer/api build \
 && pnpm --filter @dataminer/api --prod --legacy deploy /app

FROM node:22-bookworm-slim AS runtime
ENV NODE_ENV=production \
    DATAMINER_CATALOG_VALIDATOR_BIN=/usr/local/bin/dataminer-catalog-validator
WORKDIR /app
# node_modules + manifest from the deploy bundle; dist copied explicitly so the
# image never depends on the deploy step's file-selection heuristics.
COPY --from=build /app/node_modules ./node_modules
COPY --from=build /app/package.json ./package.json
COPY --from=build /repo/apps/api/dist ./dist
COPY --from=catalog-validator-build /repo/target/release/dataminer-catalog-validator /usr/local/bin/dataminer-catalog-validator
# dist/migrate.js resolves ../migrations from its own location, i.e.
# /app/migrations. Without this line the README's one-shot migration job
# (same image, command override) dies with ENOENT before touching the
# database — which is exactly how it failed on first real use.
COPY --from=build /repo/apps/api/migrations ./migrations
# One-shot production jobs use the reviewed SQL and authored DEV bundles from
# the same immutable image as the migration/API deployment. Credentials remain
# in Secret Manager and are injected only into the job that needs them.
COPY apps/api/scripts/grant-runtime-role.sql ./db-grants/grant-runtime-role.sql
COPY apps/console-api/scripts/grant-console-roles.sql ./db-grants/grant-console-roles.sql
COPY config/catalogs/dev ./catalogs/dev
EXPOSE 8080
USER node
# Default entry point is the ingestion API. The worker and migrate share this
# image via a command override (see apps/api/README.md → Production deployment).
CMD ["node", "dist/index.js"]
