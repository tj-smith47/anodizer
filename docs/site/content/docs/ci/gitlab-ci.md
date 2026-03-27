+++
title = "GitLab CI"
description = "Automate releases with GitLab CI/CD"
weight = 2
template = "docs.html"
+++

## Basic pipeline

```yaml
# .gitlab-ci.yml
release:
  stage: deploy
  image: rust:latest
  only:
    - tags
  script:
    - cargo install anodize
    - anodize release
  variables:
    GITHUB_TOKEN: $GITHUB_TOKEN
```

> **Note:** Even when using GitLab CI, releases are currently created on GitHub. GitLab release support is planned for a future version.
