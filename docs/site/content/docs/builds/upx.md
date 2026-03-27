+++
title = "UPX Compression"
description = "Compress binaries with UPX to reduce download size"
weight = 4
template = "docs.html"
+++

{% coming_soon() %}
This feature is planned for a future release. UPX integration will compress built binaries before archiving, reducing download size.
{% end %}

## Planned config

```yaml
upx:
  - enabled: true
    compress: best     # best | lzma | or custom flags
    ids: []            # filter by build IDs (empty = all)
```

## How it will work

After the build stage, anodize will run `upx` on matching binaries with the configured compression flags. If `upx` is not installed, the step is skipped with a warning (unless `required: true`).
