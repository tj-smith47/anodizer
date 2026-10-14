use anyhow::Result;

// Provider enum

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    S3,
    Gcs,
    AzBlob,
}

impl Provider {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "s3" => Ok(Provider::S3),
            "gs" | "gcs" => Ok(Provider::Gcs),
            "azblob" | "azure" => Ok(Provider::AzBlob),
            other => anyhow::bail!(
                "blobs: unknown provider '{}'. Valid providers are: s3, gs, azblob",
                other
            ),
        }
    }

    pub(crate) fn display_name(&self) -> &'static str {
        match self {
            Provider::S3 => "s3",
            Provider::Gcs => "gs",
            Provider::AzBlob => "azblob",
        }
    }
}
