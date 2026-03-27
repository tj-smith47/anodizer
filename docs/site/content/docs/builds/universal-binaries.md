+++
title = "Universal Binaries"
description = "Create macOS universal binaries (x86_64 + aarch64)"
weight = 3
template = "docs.html"
+++

{% coming_soon() %}
This feature is planned for a future release. Universal binaries will combine x86_64-apple-darwin and aarch64-apple-darwin builds into a single macOS universal binary using `lipo`.
{% end %}

## Planned config

```yaml
crates:
  - name: myapp
    universal_binaries:
      - name_template: "{{ ProjectName }}-{{ Version }}-darwin-universal"
        replace: true    # replace individual arch binaries with universal
```

## How it will work

After building both `x86_64-apple-darwin` and `aarch64-apple-darwin` targets, anodize will run `lipo -create -output` to produce a universal binary. The universal binary is registered as its own artifact for archiving and release.
