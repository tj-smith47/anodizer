//! The `targetVariant` builtin: the name suffix that tells apart builds
//! sharing an OS and architecture but differing by target variant.

use crate::archive_name::{
    AMD64_BASELINE_VARIANT, PPC64_BASELINE_VARIANT, RISCV64_BASELINE_VARIANT,
};
use crate::template::engine_adapter::JsonRegisterExt;

/// One dimension of a target variant: the template variable carrying it, the
/// text written before its value, and the values that are the toolchain
/// baseline and therefore render nothing.
struct Dimension {
    var: &'static str,
    prefix: &'static str,
    baseline: &'static [&'static str],
}

/// Every dimension, in the order the suffix lists them.
///
/// `Arm` and `Mips` carry no baseline: they render whenever set, as the
/// default archive name has always rendered them.
const DIMENSIONS: &[Dimension] = &[
    Dimension {
        var: "Arm",
        prefix: "v",
        baseline: &[],
    },
    Dimension {
        var: "Mips",
        prefix: "_",
        baseline: &[],
    },
    Dimension {
        var: "Amd64",
        prefix: "",
        baseline: &[AMD64_BASELINE_VARIANT],
    },
    Dimension {
        var: "Arm64",
        prefix: "",
        baseline: &["v8", "v8.0"],
    },
    Dimension {
        var: "I386",
        prefix: "",
        baseline: &["sse2"],
    },
    Dimension {
        var: "Ppc64",
        prefix: "",
        baseline: &[PPC64_BASELINE_VARIANT],
    },
    Dimension {
        var: "Riscv64",
        prefix: "",
        baseline: &[RISCV64_BASELINE_VARIANT],
    },
    Dimension {
        var: "Abi",
        prefix: "_",
        baseline: &[],
    },
];

/// Render the target-variant suffix from the per-target template variables
/// `lookup` resolves (an unset variable resolves to the empty string).
///
/// Each of `Arm`, `Mips`, `Amd64`, `Arm64`, `I386`, `Ppc64`, `Riscv64` and
/// `Abi` contributes its value unless it is empty or the toolchain baseline,
/// so a build that pins no variant keeps a suffix-free architecture token and
/// gains only its ABI (`_gnu`, `_musl`, `_msvc`). A scope with no target
/// renders the empty string. A comma (an `Arm64` feature list such as
/// `v9.0,lse`) is written as a dash, since the suffix is meant for file and
/// release-asset names.
pub(crate) fn target_variant_suffix(lookup: &dyn Fn(&str) -> String) -> String {
    let mut suffix = String::new();
    for dimension in DIMENSIONS {
        let value = lookup(dimension.var);
        if !value.is_empty() && !dimension.baseline.contains(&value.as_str()) {
            suffix.push_str(dimension.prefix);
            suffix.push_str(&value);
        }
    }
    suffix.replace(',', "-")
}

pub(super) fn register(tera: &mut tera::Tera) {
    tera.register_context_function("targetVariant", target_variant_suffix);
}
