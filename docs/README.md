# Documentation map

The tracked `docs/` tree contains public, durable documentation only.

| Path | Public content allowed |
| --- | --- |
| `product/` | Approved or shipped product behavior |
| `ui/` | Public UI/UX specifications and interaction rules |
| `release/` | Sanitized build, signing, packaging, and release guidance |
| `architecture/` | Approved public architecture and interoperability contracts |
| `governance/` | Repository policy and contribution boundaries |
| `images/` | Public documentation assets |

`SHARED_MODELS_CONTRACT.md` remains at the top level because another Lumen
repository verifies the same cross-repository contract byte for byte.

## Research is local-only

Research must be created under `.research/docs/<topic>/`, never under `docs/`.
The entire `.research/` tree is ignored and rejected if force-added. Use topic
directories such as:

```text
.research/docs/
  asr/
  context/
  benchmarks/
  competitive/
  platform/
  experiments/
```

ASR provider selection and experiments, Context capture/inference work, and all
benchmark tooling, data, results, and methodology are prohibited from the
public repository. A document does not become public merely because it is
renamed as a design, plan, or product specification.

Product/UI documents may be published when they describe approved behavior and
contain no internal roadmap, research evidence, private evaluation material, or
unpublished gaps. Release documents may be published only after removing
credentials, personal Apple IDs or Team IDs, certificate snapshots, private
asset URLs, and machine-specific values.

New or modified PNG, JPEG, WebP, or GIF assets under `docs/images/` require a
same-commit `<image-path>.public.md` sidecar. The sidecar records that the asset
was inspected and contains only approved public product/UI material with
synthetic or otherwise public data. It must contain these exact attestation
fields, including the current image's Git blob ID:

```text
asset-class: public-product-ui
data-class: synthetic
human-reviewed: true
asset-blob: <oid>
```

`data-class: public` is also accepted. A sidecar is not an exception mechanism.
