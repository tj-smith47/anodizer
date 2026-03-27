+++
title = "Source Archives & SBOM"
description = "Generate source archives and software bill of materials"
weight = 5
template = "docs.html"
+++

{% coming_soon() %}
Source archive generation and SBOM (Software Bill of Materials) support are planned for a future release.
{% end %}

## Planned: Source archives

Create `.tar.gz` or `.zip` archives of your source tree (respecting `.gitignore`) and attach them as release assets.

```yaml
source:
  enabled: true
  format: tar.gz    # tar.gz or zip
```

## Planned: SBOM generation

Generate a CycloneDX or SPDX SBOM from your `Cargo.lock` dependencies:

```yaml
sbom:
  enabled: true
  format: cyclonedx    # cyclonedx or spdx
```

The SBOM is attached as a release asset, providing supply chain transparency for your users.
