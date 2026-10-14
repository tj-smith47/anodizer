// triple_to_snap_arch — map target triple to snapcraft architecture name

/// Map a Rust target triple to a snapcraft architecture name.
pub(super) fn triple_to_snap_arch(triple: &str) -> &'static str {
    if triple.contains("x86_64") || triple.contains("amd64") {
        "amd64"
    } else if triple.contains("aarch64") || triple.contains("arm64") {
        "arm64"
    } else if triple.contains("armv7") {
        "armhf"
    } else if triple.contains("i686") || triple.contains("i386") || triple.contains("i586") {
        "i386"
    } else if triple.contains("s390x") {
        "s390x"
    } else if triple.contains("ppc64le") || triple.contains("powerpc64le") {
        "ppc64el"
    } else if triple.contains("riscv64") {
        // riscv64 is not supported by the snap store; mapped but filtered below
        "riscv64"
    } else {
        "amd64"
    }
}

/// check whether an architecture
/// is supported by the snap store.  riscv64 is not in the list.
pub(super) fn is_valid_snap_arch(arch: &str) -> bool {
    matches!(
        arch,
        "s390x" | "ppc64el" | "arm64" | "armhf" | "i386" | "amd64"
    )
}
