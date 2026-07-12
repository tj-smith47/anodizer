//! PyPI publisher — native binary wheels from prebuilt binaries.
//!
//! Ships the release's built binaries as Python wheels, one
//! `py3-none-<platform>` wheel per built target, with the platform tag
//! derived by *inspecting the binary* rather than guessing:
//!
//! * `*-linux-gnu` → `manylinux_<maj>_<min>_<arch>` from the binary's real
//!   glibc floor ([`anodizer_core::libc_check::max_glibc_requirement`]);
//! * `*-linux-musl` → `musllinux_1_2_<arch>`;
//! * `*-apple-darwin` → `macosx_<maj>_<min>_<arch>` from the Mach-O
//!   deployment target ([`anodizer_core::macho_check::macho_min_os_version`]),
//!   `universal2` for fat binaries;
//! * windows → `win_amd64` / `win32` / `win_arm64`.
//!
//! The wheel carries the executable under `<name>-<version>.data/scripts/`
//! so `pip install` drops it on the console-script PATH — the same layout
//! maturin's `bindings = "bin"` mode emits. An optional source distribution
//! is delegated to `maturin sdist`. Uploads speak PyPI's legacy
//! (twine-protocol) multipart API with `__token__` Basic auth.

pub(crate) mod pep;
pub mod publisher;
pub(crate) mod sdist;
pub(crate) mod upload;
pub(crate) mod wheel;

#[cfg(test)]
mod tests;

pub use publisher::{
    PypiPublisher, pypi_version_live, static_entry_crate_name, static_project_name,
    static_repository,
};
