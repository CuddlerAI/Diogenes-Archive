# syntax=docker/dockerfile:1
#
# Build @dataminer/console-api (a pnpm-workspace package) into a self-contained
# Cloud Run image. The build context MUST be the DataMiner repo root, because the
# pnpm lockfile and workspace manifest live there:
#
#   docker build -f apps/console-api/Dockerfile -t <image> .
#
# Cloud Build uses apps/console-api/cloudbuild.yaml, which sets the same context.
# The service must run with HOST=0.0.0.0 (the app defaults to 127.0.0.1, which
# would refuse Cloud Run traffic) and honours the injected PORT.

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
COPY pnpm-lock.yaml pnpm-workspace.yaml package.json ./
COPY apps/api/package.json apps/api/package.json
COPY apps/api-edge/package.json apps/api-edge/package.json
COPY apps/client/package.json apps/client/package.json
COPY apps/console-api/package.json apps/console-api/package.json
COPY apps/console-web/package.json apps/console-web/package.json
COPY apps/mirror/package.json apps/mirror/package.json

# Only console-api and its dependencies, and only production deps in the bundle.
RUN pnpm install --frozen-lockfile --filter @dataminer/console-api...

# console-api imports @dataminer/mirror/credentials, whose export is TypeScript
# source (../mirror/src/credentials.ts), so the mirror package's source must be
# present for the build and for the runtime bundle (Node >=22.18 strips its types
# on load). Copying only its manifest above is enough to link it; the source is
# needed here.
COPY apps/api apps/api
COPY apps/mirror apps/mirror
COPY apps/console-api apps/console-api
# Build, then produce a self-contained deploy bundle with a real (non-symlinked)
# node_modules under /app.
RUN pnpm --filter @dataminer/api build \
 && pnpm --filter @dataminer/mirror build \
 && pnpm --filter @dataminer/console-api build \
 && pnpm --filter @dataminer/console-api --prod --legacy deploy /app

FROM node:22-bookworm-slim AS runtime
ENV NODE_ENV=production \
    DATAMINER_CATALOG_VALIDATOR_BIN=/usr/local/bin/dataminer-catalog-validator
WORKDIR /app
# node_modules + manifest from the deploy bundle; dist copied explicitly so the
# image never depends on the deploy step's file-selection heuristics.
COPY --from=build /app/node_modules ./node_modules
COPY --from=build /app/package.json ./package.json
COPY --from=build /repo/apps/console-api/dist ./dist
COPY --from=catalog-validator-build /repo/target/release/dataminer-catalog-validator /usr/local/bin/dataminer-catalog-validator
# The consent text is read from disk at startup (CONSOLE_APP_TERMS_BODY_PATH) and
# its hash is checked against CONSOLE_APP_TERMS_HASH, so the file has to be in the
# runtime image — not just in the build stage.
COPY --from=build /repo/apps/console-api/terms ./terms
EXPOSE 8080
USER node
CMD ["node", "dist/index.js"]
