# Deploying forge-gateway

`forge-gateway` is the gRPC daemon serving `forge.v1.ForgeService` over the
crate's GitHub/GitLab adapters — the single source of truth for forge write ops.
It is **creds-less**: each caller's identity travels in request metadata
(`x-fastverk-gitlab-token`/`-host`, `x-fastverk-github-token`), so every op runs
as the caller. Consumers: the console forge plugin's MR write MCP tools (via
`FORGE_GATEWAY_ADDR`) and the `wave` cascade engine.

## What's in this repo

- **Binary**: `//:forge-gateway` (`src/bin/forge-gateway.rs` → `forge::gateway`).
  Bind address `$FORGE_GATEWAY_BIND` (default `0.0.0.0:50055`).
- **Chart**: `deploy/charts/forge-gateway` (Deployment + Service, ClusterIP,
  gRPC :50055, tcpSocket probes). `helm lint` clean.
- **ArgoCD app**: `fastverk-deploy/argocd/apps/forge-gateway.yaml` (pulls the
  chart from the ECR OCI chart registry, image tag baked at package time).

## Remaining onboarding step: the OCI image

The forge repo isn't yet wired for container images. Add it exactly like the
sibling `agents` repo (which ported the house macro from botnoc):

1. **`MODULE.bazel`** — add the image deps + the distroless/cc base (the zig
   `gnu.2.28` binaries link glibc dynamically, so a static base won't link).
   Copy the block verbatim from `agents/MODULE.bazel` (digests are proven there):
   ```starlark
   bazel_dep(name = "aspect_bazel_lib", version = "2.22.5")
   bazel_dep(name = "rules_oci", version = "2.2.6")
   bazel_dep(name = "rules_pkg", version = "1.0.1")
   oci = use_extension("@rules_oci//oci:extensions.bzl", "oci")
   oci.pull(
       name = "distroless_cc",
       digest = "sha256:6714977f9f02632c31377650c15d89a7efaebf43bab0f37c712c30fc01edb973",
       image = "gcr.io/distroless/cc-debian12",
       platforms = ["linux/amd64"],
   )
   use_repo(oci, "distroless_cc", "distroless_cc_linux_amd64")
   ```
2. **`tools/oci/`** — copy `agents/tools/oci/defs.bzl` (`rust_service_image`) and
   its `BUILD.bazel` (the `base_image` alias → `@distroless_cc` and the
   `linux_amd64` platform) verbatim.
3. **`BUILD.bazel`** — package the gateway:
   ```starlark
   load("//tools/oci:defs.bzl", "rust_service_image")
   rust_service_image(
       name = "forge-gateway-image",
       binary = ":forge-gateway",
       repository = "042825952740.dkr.ecr.us-east-1.amazonaws.com/forge-gateway",
       exposed_ports = ["50055/tcp"],
   )
   ```
4. **Build/push** on RBE (linux worker): the build-runner runs
   `bazel run //:forge-gateway-image_push --config=rbe -- --repository <ECR>/forge-gateway --tag <sha>`
   and `helm package deploy/charts/forge-gateway --version 0.1.0-<sha> --set image.tag=<sha>`,
   pushes the chart to the ECR chart registry, and writes `<sha>` into the
   ArgoCD app's `targetRevision`. (Onboard this repo to the build-runner so a
   push triggers a BuildRun, same as botnoc.)

## Wire the consumers

- **Console forge plugin**: set `forgeGatewayAddr` in the `botnoc-forge` chart
  values to `http://forge-gateway.fastverk.svc:50055` (renders
  `FORGE_GATEWAY_ADDR`). Its MR write MCP tools then reach the gateway; unset,
  they return a clear "not configured" error and the read tools keep working.
- **wave**: migrate from the in-process `Forge` trait to a `ForgeService` client
  dialing the gateway (follow-up; the trait path keeps working meanwhile).
