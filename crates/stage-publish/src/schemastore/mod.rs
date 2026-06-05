//! SchemaStore publisher: pure helpers (slug, description validation) and
//! future catalog-entry construction and JSON vendor formatting.

pub(crate) mod catalog;
pub(crate) mod manifest;
pub(crate) mod scan;

#[cfg(test)]
mod tests;
