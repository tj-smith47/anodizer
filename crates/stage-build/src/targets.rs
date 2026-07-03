// ---------------------------------------------------------------------------
// Build ignore/override helpers
// ---------------------------------------------------------------------------

// The glob/env/override resolution semantics live in core
// (`anodizer_core::build_env`) so this stage and the config-time asset-name
// derivation (which must project the same per-target env to derive the amd64
// micro-arch level) share one implementation.
pub(crate) use anodizer_core::build_env::{
    find_matching_override, is_target_ignored, resolve_target_env,
};

// ---------------------------------------------------------------------------
// Known Rust target triples (Tier 1 + Tier 2) for validation
// ---------------------------------------------------------------------------

pub(crate) const KNOWN_TARGETS: &[&str] = &[
    // Tier 1
    "aarch64-unknown-linux-gnu",
    "i686-pc-windows-gnu",
    "i686-pc-windows-msvc",
    "i686-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-gnu",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    // Tier 2 with host tools
    "aarch64-apple-darwin",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-linux-musl",
    "arm-unknown-linux-gnueabi",
    "arm-unknown-linux-gnueabihf",
    "armv7-unknown-linux-gnueabihf",
    "loongarch64-unknown-linux-gnu",
    "loongarch64-unknown-linux-musl",
    "powerpc-unknown-linux-gnu",
    "powerpc64-unknown-linux-gnu",
    "powerpc64le-unknown-linux-gnu",
    "riscv64gc-unknown-linux-gnu",
    "s390x-unknown-linux-gnu",
    "x86_64-unknown-freebsd",
    "x86_64-unknown-illumos",
    "x86_64-unknown-linux-musl",
    "x86_64-unknown-netbsd",
    // Common Tier 2
    "aarch64-linux-android",
    "aarch64-unknown-linux-ohos",
    "armv7-linux-androideabi",
    "i686-linux-android",
    "i686-unknown-linux-musl",
    "thumbv7neon-unknown-linux-gnueabihf",
    "wasm32-unknown-unknown",
    "wasm32-wasi",
    "wasm32-wasip1",
    "wasm32-wasip2",
    "x86_64-linux-android",
    "x86_64-apple-ios",
    "aarch64-apple-ios",
    "aarch64-apple-ios-sim",
    // MIPS targets
    "mips-unknown-linux-gnu",
    "mips-unknown-linux-musl",
    "mipsel-unknown-linux-gnu",
    "mipsel-unknown-linux-musl",
    "mips64-unknown-linux-gnuabi64",
    "mips64-unknown-linux-muslabi64",
    "mips64el-unknown-linux-gnuabi64",
    "mips64el-unknown-linux-muslabi64",
    // RISC-V targets
    "riscv32i-unknown-none-elf",
    "riscv32imac-unknown-none-elf",
    "riscv32imc-unknown-none-elf",
    "riscv64gc-unknown-linux-musl",
    "riscv64gc-unknown-none-elf",
    "riscv64imac-unknown-none-elf",
    // s390x targets
    "s390x-unknown-linux-musl",
    // PowerPC targets
    "powerpc-unknown-linux-gnuspe",
    "powerpc64le-unknown-linux-musl",
    "powerpc64-unknown-linux-musl",
    // SPARC targets
    "sparc64-unknown-linux-gnu",
    "sparc-unknown-linux-gnu",
    "sparcv9-sun-solaris",
    // Thumb targets (ARM embedded)
    "thumbv6m-none-eabi",
    "thumbv7em-none-eabi",
    "thumbv7em-none-eabihf",
    "thumbv7m-none-eabi",
    "thumbv7neon-linux-androideabi",
    "thumbv8m.base-none-eabi",
    "thumbv8m.main-none-eabi",
    "thumbv8m.main-none-eabihf",
    // i686 targets
    "i686-unknown-freebsd",
    "i686-unknown-linux-musl",
    // Additional i686 targets
    "i586-unknown-linux-gnu",
    "i586-unknown-linux-musl",
    // Additional ARM targets
    "arm-unknown-linux-musleabi",
    "arm-unknown-linux-musleabihf",
    "armv5te-unknown-linux-gnueabi",
    "armv5te-unknown-linux-musleabi",
    "armv7-unknown-linux-gnueabi",
    "armv7-unknown-linux-musleabi",
    "armv7-unknown-linux-musleabihf",
    "armv7-unknown-linux-ohos",
    // Apple targets
    "aarch64-apple-tvos",
    "aarch64-apple-watchos",
    // FreeBSD / OpenBSD / NetBSD
    "aarch64-unknown-freebsd",
    "x86_64-unknown-openbsd",
    "aarch64-unknown-openbsd",
    // Solaris / illumos
    "x86_64-pc-solaris",
    // Fuchsia
    "aarch64-unknown-fuchsia",
    "x86_64-unknown-fuchsia",
    // Redox
    "x86_64-unknown-redox",
    // Haiku
    "x86_64-unknown-haiku",
    // UEFI
    "x86_64-unknown-uefi",
    "aarch64-unknown-uefi",
    "i686-unknown-uefi",
    // None / bare-metal
    "aarch64-unknown-none",
    "aarch64-unknown-none-softfloat",
    "x86_64-unknown-none",
];
