use super::*;

#[cfg(test)]
mod faked_time_tests {
    use super::inject_gpg_faked_system_time;
    use anodizer_core::MapEnvSource;

    fn env_with_sde(value: &str) -> MapEnvSource {
        MapEnvSource::new().with("SOURCE_DATE_EPOCH", value)
    }

    fn env_without_sde() -> MapEnvSource {
        MapEnvSource::new()
    }

    #[test]
    fn injects_after_first_arg_for_gpg_with_sde() {
        let env = env_with_sde("1715000000");
        let mut args = vec![
            "--batch".into(),
            "--local-user".into(),
            "ABCD".into(),
            "--detach-sig".into(),
            "file".into(),
        ];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        assert_eq!(args[0], "--batch");
        assert_eq!(args[1], "--faked-system-time=1715000000!");
        assert_eq!(args[2], "--local-user");
    }

    #[test]
    fn no_inject_when_sde_unset() {
        let env = env_without_sde();
        let mut args = vec!["--batch".into(), "--detach-sig".into()];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        assert_eq!(args, vec!["--batch".to_string(), "--detach-sig".into()]);
    }

    #[test]
    fn no_inject_when_cmd_is_not_gpg() {
        let env = env_with_sde("1715000000");
        let mut args = vec!["sign-blob".into(), "--key=env://KEY".into()];
        inject_gpg_faked_system_time("cosign", &mut args, &env);
        assert_eq!(
            args,
            vec!["sign-blob".to_string(), "--key=env://KEY".into()]
        );
    }

    #[test]
    fn no_inject_when_user_already_passed_faked_system_time() {
        let env = env_with_sde("1715000000");
        let mut args = vec![
            "--batch".into(),
            "--faked-system-time=999!".into(),
            "--detach-sig".into(),
        ];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        let count = args
            .iter()
            .filter(|a| a.starts_with("--faked-system-time"))
            .count();
        assert_eq!(count, 1);
        assert_eq!(args[1], "--faked-system-time=999!");
    }

    #[test]
    fn injects_for_gpg2_and_absolute_gpg_path() {
        // Regression: the injection once required cmd == "gpg" EXACT, so
        // gpg2/absolute-path configs silently produced timestamp-drifting
        // signatures under SOURCE_DATE_EPOCH.
        for cmd in ["gpg2", "/usr/bin/gpg"] {
            let env = env_with_sde("1715000000");
            let mut args = vec!["--batch".into(), "--detach-sig".into()];
            inject_gpg_faked_system_time(cmd, &mut args, &env);
            assert_eq!(
                args[1], "--faked-system-time=1715000000!",
                "injection must fire for cmd '{cmd}'"
            );
        }
    }

    #[test]
    fn injects_at_position_zero_when_args_empty() {
        let env = env_with_sde("42");
        let mut args: Vec<String> = vec![];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        assert_eq!(args, vec!["--faked-system-time=42!".to_string()]);
    }
}

#[cfg(test)]
mod harden_cosign_tests {
    use super::harden_cosign_args_for_harness;
    use anodizer_core::MapEnvSource;
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    /// Build a Context whose injected env carries (or omits) the harness marker.
    fn ctx_with_harness(harness: bool) -> Context {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        let env = if harness {
            MapEnvSource::new().with("ANODIZER_IN_DETERMINISM_HARNESS", "1")
        } else {
            MapEnvSource::new()
        };
        ctx.set_env_source(env);
        ctx
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn appends_tlog_false_for_keyed_cosign_under_harness() {
        let ctx = ctx_with_harness(true);
        let out = harden_cosign_args_for_harness(
            "cosign",
            args(&[
                "sign-blob",
                "--key=env://COSIGN_KEY",
                "--bundle=cosign.bundle",
                "--yes",
                "artifact",
            ]),
            &ctx,
        );
        assert_eq!(out.last().map(String::as_str), Some("--tlog-upload=false"));
        assert_eq!(
            out.iter()
                .filter(|a| a.starts_with("--tlog-upload"))
                .count(),
            1,
            "appended exactly once: {out:?}"
        );
    }

    #[test]
    fn unchanged_when_not_under_harness() {
        let ctx = ctx_with_harness(false);
        let input = args(&["sign-blob", "--key=env://COSIGN_KEY", "artifact"]);
        let out = harden_cosign_args_for_harness("cosign", input.clone(), &ctx);
        assert_eq!(out, input);
    }

    #[test]
    fn unchanged_for_non_cosign_cmd() {
        let ctx = ctx_with_harness(true);
        let input = args(&["--detach-sig", "--key=secret", "artifact"]);
        let out = harden_cosign_args_for_harness("gpg", input.clone(), &ctx);
        assert_eq!(out, input);
    }

    #[test]
    fn unchanged_for_keyless_cosign() {
        let ctx = ctx_with_harness(true);
        let input = args(&["sign-blob", "--bundle=cosign.bundle", "--yes", "artifact"]);
        let out = harden_cosign_args_for_harness("cosign", input.clone(), &ctx);
        assert_eq!(out, input);
    }

    #[test]
    fn unchanged_when_tlog_already_pinned() {
        let ctx = ctx_with_harness(true);

        let eq_true = args(&["sign-blob", "--key=k", "--tlog-upload=true", "a"]);
        assert_eq!(
            harden_cosign_args_for_harness("cosign", eq_true.clone(), &ctx),
            eq_true
        );

        let two_token = args(&["sign-blob", "--key=k", "--tlog-upload", "false", "a"]);
        assert_eq!(
            harden_cosign_args_for_harness("cosign", two_token.clone(), &ctx),
            two_token
        );

        let eq_false = args(&["sign-blob", "--key=k", "--tlog-upload=false", "a"]);
        let out = harden_cosign_args_for_harness("cosign", eq_false, &ctx);
        assert_eq!(
            out.iter()
                .filter(|a| a.starts_with("--tlog-upload"))
                .count(),
            1,
            "no duplicate when already pinned false: {out:?}"
        );
    }

    #[test]
    fn matches_cosign_basename_through_path() {
        let ctx = ctx_with_harness(true);
        let out = harden_cosign_args_for_harness(
            "/usr/local/bin/cosign",
            args(&["sign-blob", "--key=env://COSIGN_KEY", "artifact"]),
            &ctx,
        );
        assert_eq!(out.last().map(String::as_str), Some("--tlog-upload=false"));
    }

    #[test]
    fn appends_tlog_false_for_two_token_key_form() {
        let ctx = ctx_with_harness(true);
        let out = harden_cosign_args_for_harness(
            "cosign",
            args(&[
                "sign-blob",
                "--key",
                "env://COSIGN_KEY",
                "--yes",
                "artifact",
            ]),
            &ctx,
        );
        assert_eq!(out.last().map(String::as_str), Some("--tlog-upload=false"));
    }
}
