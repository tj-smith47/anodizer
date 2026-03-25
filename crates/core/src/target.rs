// Build target types — stub, to be implemented.

pub fn map_target(triple: &str) -> (String, String) {
    let parts: Vec<&str> = triple.split('-').collect();
    let arch = match parts.first().copied().unwrap_or("unknown") {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "i686" => "386",
        "armv7" => "armv7",
        other => other,
    };
    let os = if triple.contains("linux") {
        "linux"
    } else if triple.contains("darwin") || triple.contains("apple") {
        "darwin"
    } else if triple.contains("windows") {
        "windows"
    } else if triple.contains("freebsd") {
        "freebsd"
    } else {
        "unknown"
    };
    (os.to_string(), arch.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_to_os_arch() {
        let (os, arch) = map_target("x86_64-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "amd64");
    }

    #[test]
    fn test_darwin_arm64() {
        let (os, arch) = map_target("aarch64-apple-darwin");
        assert_eq!(os, "darwin");
        assert_eq!(arch, "arm64");
    }

    #[test]
    fn test_windows() {
        let (os, arch) = map_target("x86_64-pc-windows-msvc");
        assert_eq!(os, "windows");
        assert_eq!(arch, "amd64");
    }

    #[test]
    fn test_unknown_target() {
        let (os, arch) = map_target("riscv64gc-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "riscv64gc");
    }
}
