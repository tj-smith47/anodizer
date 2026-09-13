//! The release-asset NAME a `binary_signs:` output uploads under.
//!
//! One derivation, read by the sign stage and by the release gate: a name
//! read back from a registered archive differs between `anodizer build`
//! (which signs before any archive exists) and `anodizer release
//! --publish-only` (whose registry is the preserved manifest). The claim map
//! refuses a run in which two outputs would upload one asset name.

use std::collections::HashMap;

use anyhow::{Context as _, Result};

use anodizer_core::config::SignConfig;
use anodizer_core::context::Context;

/// Append a target triple to a basename while keeping its extension
/// suffix: `anodizer.sig` + `aarch64-apple-darwin` →
/// `anodizer-aarch64-apple-darwin.sig`, `anodizer.exe.sig` →
/// `anodizer.exe-aarch64-pc-windows-msvc.sig`. A basename with no
/// extension gets a plain `-<target>` suffix.
pub(crate) fn qualify_basename_with_target(name: &str, target: &str) -> String {
    let path = std::path::Path::new(name);
    match (
        path.file_stem().and_then(|s| s.to_str()),
        path.extension().and_then(|e| e.to_str()),
    ) {
        (Some(stem), Some(ext)) => format!("{stem}-{target}.{ext}"),
        _ => format!("{name}-{target}"),
    }
}

/// The name template a binary signature falls back to when NO `archives:`
/// entry covers the binary's target — a musl build that only feeds npm, say.
///
/// The full triple is required: `Os`/`Arch` render identically for a gnu and
/// a musl build of one machine, so a `{{ Os }}-{{ Arch }}` name would collapse
/// the two targets' signatures onto one asset. The amd64 micro-architecture
/// level is the dimension the triple itself does not carry — a baseline and a
/// `-Ctarget-cpu=x86-64-v3` build share `x86_64-unknown-linux-gnu` — so the
/// tail every default name template appends
/// ([`anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX`]) is
/// appended here too; `v1` renders nothing, so an ordinary build keeps its
/// historical name.
pub(crate) const UNCOVERED_TARGET_NAME_TEMPLATE: &str = concat!(
    "{{ Binary }}-{{ Version }}-{{ Target }}",
    "{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
);

/// The rendered base of a binary signature asset together with the template
/// it came from, so a diagnostic about the name can quote the template the
/// operator has to change.
pub(crate) struct BinarySignNaming {
    /// The rendered asset base.
    pub(crate) base: String,
    /// The template that rendered it.
    pub(crate) template: String,
}

/// The release-asset BASE name every signature and certificate of one raw
/// binary is built on — unique per (crate, target, binary).
///
/// Derived from CONFIG alone, never from which archives the run has
/// registered, so `anodizer build` (which signs before any archive exists)
/// and `anodizer release --publish-only` (whose registry is the preserved
/// manifest) name the same binary's signature identically:
///
/// | the binary's target | base |
/// |---|---|
/// | covered by the crate's primary `archives:` entry — the first entry in config order whose `ids:` / `binaries:` filters take this binary — and that entry packs this binary alone on the target | that entry's `name_template`, rendered in the archive stage's own per-target scope ([`anodizer_core::archive_name::seed_archive_name_vars`]) |
/// | covered by an entry that packs SEVERAL binaries on the target, or by no entry at all | [`UNCOVERED_TARGET_NAME_TEMPLATE`] |
/// | covered by an entry whose RESOLVED formats include `binary` | that executable's own asset name (the per-binary default template plus the Windows `.exe`), so a `signs:` signature over the uploaded binary and a `binary_signs:` signature over the same bytes resolve to one name |
/// | a lipo-merged universal binary (`darwin-universal`, which no `builds:` entry names) | the covering entry's `name_template` rendered with the binary's OWN name |
///
/// The entry's `if:` is deliberately NOT evaluated: a gate that reads the
/// environment would name one binary's signature differently on the machine
/// that builds it and the machine that publishes it.
///
/// A `binary_signs:` entry's `asset_name_template:` overrides every row.
pub(crate) fn binary_sign_asset_naming(
    ctx: &Context,
    cfg: &SignConfig,
    binary: &anodizer_core::artifact::Artifact,
    target: &str,
) -> Result<BinarySignNaming> {
    use anodizer_core::archive_name;

    let binary_name = binary.binary_name().unwrap_or_default();
    let amd64_variant = binary.metadata.get("amd64_variant").map(String::as_str);
    // The archive stage rebinds `ProjectName` to the per-crate name whenever
    // its work list holds more than one crate, and picks the multi-crate
    // default template on the same condition. Answered from config, since the
    // registry-aware answer differs between a build and a publish-only run.
    let multi_crate = archive_name::config_archives_more_than_one_crate(ctx);
    let seeded = |binary_var: &str| {
        let mut vars = ctx.template_vars().clone();
        if multi_crate {
            vars.set("ProjectName", &binary.crate_name);
        }
        archive_name::seed_archive_name_vars(
            &mut vars,
            target,
            &binary.crate_name,
            binary_var,
            amd64_variant,
        );
        vars
    };
    let render = |template: &str, vars: &anodizer_core::template::TemplateVars| {
        anodizer_core::template::render(template, vars).with_context(|| {
            format!(
                "sign: render binary signature asset name '{template}' for \
                 {}/{target}",
                binary.crate_name
            )
        })
    };

    let named = |base: Result<String>, template: &str| -> Result<BinarySignNaming> {
        Ok(BinarySignNaming {
            base: reject_empty_base(base?)?,
            template: template.to_string(),
        })
    };

    if let Some(template) = cfg.asset_name_template.as_deref() {
        return named(render(template, &seeded(&binary_name)), template);
    }

    let krate = ctx.config.find_crate(&binary.crate_name);
    let entry = krate
        .map(anodizer_core::archive_selection::effective_archive_configs)
        .unwrap_or_default()
        .into_iter()
        .find(|c| {
            // A meta entry packs no binaries, so the archive stage names it
            // with an EMPTY `{{ Binary }}` — a base derived from it names an
            // asset this binary never appears in.
            !c.meta.unwrap_or(false)
                && anodizer_core::artifact::matches_id_filter(binary, c.ids.as_deref())
                && c.binaries
                    .as_deref()
                    .is_none_or(|names| names.contains(&binary_name))
        });

    let Some(entry) = entry else {
        return named(
            render(UNCOVERED_TARGET_NAME_TEMPLATE, &seeded(&binary_name)),
            UNCOVERED_TARGET_NAME_TEMPLATE,
        );
    };

    let formats = archive_name::archive_formats_for_target(
        &entry,
        target,
        &archive_name::global_format_overrides(ctx),
        &archive_name::global_default_archive_format(ctx),
    );
    // An entry may produce several formats at once; whenever `binary` is one
    // of them the executable is itself an uploaded asset, and the signature
    // over its bytes takes that asset's name.
    let is_binary_format = formats
        .iter()
        .any(|f| f == anodizer_core::artifact::FORMAT_BINARY);
    let template = entry.name_template.clone().unwrap_or_else(|| {
        if is_binary_format {
            archive_name::DEFAULT_BINARY_NAME_TEMPLATE.to_string()
        } else if multi_crate {
            // Chosen from the same config-only answer that rebinds
            // `ProjectName` above; the registry-aware resolver would pick one
            // default on a build and another on a publish-only run.
            archive_name::DEFAULT_NAME_TEMPLATE_MULTI_CRATE.to_string()
        } else {
            archive_name::DEFAULT_NAME_TEMPLATE.to_string()
        }
    });
    if is_binary_format {
        // Each executable is published under its own name, so the group holds
        // no ambiguity to resolve. The stem is checked before the extension is
        // appended: a bare `.exe` names no subject either.
        let stem = reject_empty_base(render(&template, &seeded(&binary_name))?)?;
        return Ok(BinarySignNaming {
            base: archive_name::binary_output_name(stem, target),
            template,
        });
    }

    let packed: Vec<String> = krate
        .map(|krate| {
            anodizer_core::build_plan::archive_target_binaries(
                krate,
                entry.ids.as_deref(),
                entry.binaries.as_deref(),
                target,
                &ctx.config.effective_default_targets(),
                |t| ctx.render_template(t),
            )
        })
        .unwrap_or_default();
    // One archive carries the whole group under a single name, so several
    // packed binaries cannot each be named after it — the release would keep
    // one signature and silently drop its siblings.
    if packed.len() > 1 {
        return named(
            render(UNCOVERED_TARGET_NAME_TEMPLATE, &seeded(&binary_name)),
            UNCOVERED_TARGET_NAME_TEMPLATE,
        );
    }
    let binary_var = packed
        .into_iter()
        .next()
        .unwrap_or_else(|| binary_name.clone());
    named(render(&template, &seeded(&binary_var)), &template)
}

/// Reject an empty rendered base before it becomes a `.sig` asset with no
/// stem — a name the release cannot match to its subject and that collides
/// with every other binary's signature.
fn reject_empty_base(base: String) -> Result<String> {
    if base.is_empty() {
        anyhow::bail!(
            "sign: the binary signature asset name rendered empty. An empty \
             base uploads the signature as a bare `.sig`, which every other \
             binary's signature collides with. Verify the templates it is \
             rendered from (`binary_signs[].asset_name_template`, or the \
             covering `archives[].name_template`) reference variables this run \
             populates — `{{{{ Tag }}}}` is unset under `--snapshot`, use \
             `{{{{ Version }}}}`."
        );
    }
    Ok(base)
}

/// Which raw binary, and which of its outputs, claimed each signature asset
/// name in this run.
///
/// One release asset carries one file, so two outputs resolving to one asset
/// name upload two files under one name and the release keeps whichever
/// arrived last. The default templates separate every binary by target and
/// micro-architecture level, but a hand-written `archives[].name_template`
/// need not — `{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}` renders
/// one name for a baseline and a `x86-64-v3` build of the same binary.
#[derive(Default)]
pub(crate) struct BinarySignAssetNames {
    claimed: HashMap<String, ClaimedAssetName>,
}

/// What one `binary_signs:` output put behind an asset name.
///
/// The derivation travels with the claim because a collision names the
/// template each SIDE can be changed in, and the two sides need not share
/// one: a base-derived signature collides with a renamed certificate. The
/// resolved path travels with it because two entries trading a component
/// between the base and the output template's suffix resolve to one name
/// over two distinct files.
struct ClaimedAssetName {
    identity: BinaryIdentity,
    output: &'static str,
    source: AssetNameSource,
    base: String,
    template: String,
    path: std::path::PathBuf,
}

impl ClaimedAssetName {
    fn derivation(&self) -> String {
        derivation(self.output, self.source, &self.base, &self.template)
    }
}

/// Whether two rendered output paths name one file, compared the way
/// [`crate::helpers::dist_joined`] decides what is already under `dist`:
/// `./dist/x`, `dist/x` and `dist/../dist/x` are one file, so a claim keyed
/// on the textual spelling would refuse a pair the filesystem accepts.
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (
        crate::helpers::lexical_absolute(a),
        crate::helpers::lexical_absolute(b),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

/// How one claim's asset name was produced, worded for a collision message.
fn derivation(output: &str, source: AssetNameSource, base: &str, template: &str) -> String {
    match source {
        AssetNameSource::Base => format!(
            "the {output} is the base '{base}' rendered from the template \
             '{template}' plus the suffix `binary_signs[].{output}:` appended"
        ),
        AssetNameSource::OutputTemplate => format!(
            "the {output} was rendered by the `binary_signs[].{output}:` \
             template, which renamed the output instead of suffixing the \
             binary's own file name, so the asset base '{base}' (from \
             '{template}') is not part of it"
        ),
    }
}

/// The template one side of a collision can be separated in.
fn remedy(output: &str, source: AssetNameSource) -> String {
    match source {
        AssetNameSource::Base => "give the covering `archives[].name_template` a variable that \
             separates them ({{ Target }} and {{ Amd64 }} are the dimensions \
             {{ Os }}-{{ Arch }} drops), or set \
             `binary_signs[].asset_name_template`"
            .to_string(),
        AssetNameSource::OutputTemplate => format!(
            "give the `binary_signs[].{output}:` template {{{{ .Artifact }}}} \
             or the target, so it renders one name per binary"
        ),
    }
}

/// How the artifact registry tells one raw binary from another.
///
/// Not the file path: two `builds:` entries differing only in
/// `amd64_variant:` compile to ONE path, and `no_unique_dist_dir: true`
/// flattens every binary of a crate onto `dist/<file name>`. The registry
/// separates them by the build entry they came from, the target they were
/// compiled for and the executable they carry.
#[derive(Clone, PartialEq, Eq)]
struct BinaryIdentity {
    crate_name: String,
    build_id: String,
    target: String,
    binary: String,
    amd64_variant: String,
    path: String,
}

impl BinaryIdentity {
    fn of(binary: &anodizer_core::artifact::Artifact) -> Self {
        let meta = |key: &str| binary.metadata.get(key).cloned();
        Self {
            crate_name: binary.crate_name.clone(),
            build_id: meta("id").unwrap_or_else(|| binary.name.clone()),
            target: binary.target.clone().unwrap_or_default(),
            binary: meta("binary").unwrap_or_else(|| binary.name.clone()),
            // An absent level IS the baseline, so a v1 build and a v3 build
            // of one binary are two identities rather than one.
            amd64_variant: meta("amd64_variant").unwrap_or_else(|| "v1".to_string()),
            path: binary.path.display().to_string(),
        }
    }
}

impl std::fmt::Display for BinaryIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (crate '{}', build id '{}', target {}, amd64 {})",
            self.path, self.crate_name, self.build_id, self.target, self.amd64_variant
        )
    }
}

/// Which derivation produced a binary signature asset name.
///
/// A collision diagnostic has to name the template the operator can
/// actually change, and the two derivations answer differently: only one of
/// them reads the config-derived base at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AssetNameSource {
    /// `<base><suffix>`: the config-derived base carries the identity, and
    /// the `signature:` / `certificate:` template only appended to it.
    Base,
    /// The `signature:` / `certificate:` template rendered a name of its own
    /// rather than suffixing the binary's file name, so the base does not
    /// appear in the result and only that template separates two binaries.
    OutputTemplate,
}

impl BinarySignAssetNames {
    /// Record `asset_name` as claimed by one output of one binary, or refuse
    /// the run when the same name is already claimed by another binary, by
    /// the binary's other output, or by the same output over another file.
    ///
    /// `output` names the asset in the message (`signature` or
    /// `certificate`) and `source` decides the remedy. Both outputs are built
    /// from one base, so a certificate collides on a pair of configs whose
    /// `signature:` suffixes differ, and one entry whose two output templates
    /// render one suffix collides with itself. `path` is the file the output
    /// resolved to: one asset carries one file, so a re-claim is the same
    /// asset only while the path agrees.
    pub(crate) fn claim(
        &mut self,
        asset_name: &str,
        output: &'static str,
        naming: &BinarySignNaming,
        source: AssetNameSource,
        path: &std::path::Path,
        binary: &anodizer_core::artifact::Artifact,
    ) -> Result<()> {
        let claimant = BinaryIdentity::of(binary);
        // Two sides deriving their name the same way say it once.
        let derivations = |first: &ClaimedAssetName| {
            let mut clauses = vec![
                first.derivation(),
                derivation(output, source, &naming.base, &naming.template),
            ];
            clauses.dedup();
            clauses.join(" and ")
        };
        match self.claimed.get(asset_name) {
            Some(first) if first.identity != claimant => {
                let mut remedies = vec![remedy(first.output, first.source), remedy(output, source)];
                remedies.dedup();
                anyhow::bail!(
                    "sign: the {first_output} of '{first_identity}' and the \
                     {output} of '{claimant}' both resolve to the asset name \
                     '{asset_name}' — {derivations}. One release asset cannot \
                     carry both files — {remedies}.",
                    first_output = first.output,
                    first_identity = first.identity,
                    derivations = derivations(first),
                    remedies = remedies.join("; or "),
                )
            }
            // One binary's signature and certificate are two release assets,
            // so one name for both drops a file the gate's de-duplication
            // then folds into a single expectation it finds.
            Some(first) if first.output != output => anyhow::bail!(
                "sign: the {first_output} and the {output} of '{claimant}' \
                 both resolve to the asset name '{asset_name}' — \
                 {derivations}. One release asset cannot carry both files — \
                 give `binary_signs[].{first_output}:` and \
                 `binary_signs[].{output}:` names that differ.",
                first_output = first.output,
                derivations = derivations(first),
            ),
            // `asset_name_template:` is per entry and the asset name is
            // `<base><suffix>`, so two entries trading a component between
            // the two resolve to one name over two files — of which the
            // release keeps whichever upload arrived last.
            Some(first) if !same_file(&first.path, path) => anyhow::bail!(
                "sign: two `binary_signs:` entries resolve the {output} of \
                 '{claimant}' to one asset name '{asset_name}' over two files \
                 ('{first_path}' and '{path}'). One release asset carries one \
                 file — give the two entries `{output}:` suffixes that differ, \
                 or one of them its own `asset_name_template`.",
                first_path = first.path.display(),
                path = path.display(),
            ),
            Some(_) => Ok(()),
            None => {
                self.claimed.insert(
                    asset_name.to_string(),
                    ClaimedAssetName {
                        identity: claimant,
                        output,
                        source,
                        base: naming.base.clone(),
                        template: naming.template.clone(),
                        path: path.to_path_buf(),
                    },
                );
                Ok(())
            }
        }
    }
}

/// The release-asset name a `binary_signs:` output registers under: the
/// config-derived [`binary_sign_asset_naming`] base plus the suffix the
/// `signature:` / `certificate:` template appended to the binary's own file
/// name.
///
/// The raw binary is called the same thing under every target's directory
/// (`anodizer.sig` eight times over), so the base carries the target.
/// `anodizer.exe` → `anodizer.exe.sig` yields `<base>.sig` and a cosign bundle
/// `anodizer.bundle.sig` yields `<base>.bundle.sig`.
///
/// Falls back to the target-qualified basename when the template renamed the
/// file rather than suffixing it — still unique per target. The returned
/// [`AssetNameSource`] says which of the two produced the name, so a
/// collision diagnostic quotes a template the operator can change.
pub(crate) fn binary_sign_asset_name(
    rendered_basename: &str,
    binary_basename: &str,
    base: &str,
    target: &str,
) -> (String, AssetNameSource) {
    let renamed = || {
        (
            qualify_basename_with_target(rendered_basename, target),
            AssetNameSource::OutputTemplate,
        )
    };
    if binary_basename.is_empty() {
        return renamed();
    }
    match rendered_basename.strip_prefix(binary_basename) {
        Some(suffix) if !suffix.is_empty() => (format!("{base}{suffix}"), AssetNameSource::Base),
        _ => renamed(),
    }
}

#[cfg(test)]
mod binary_sign_asset_name_tests {
    use super::{AssetNameSource, binary_sign_asset_name, binary_sign_asset_naming};
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        ArchiveConfig, ArchivesConfig, BuildConfig, CrateConfig, SignConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const MUSL: &str = "x86_64-unknown-linux-musl";
    const WINDOWS: &str = "x86_64-pc-windows-msvc";
    const TEMPLATE: &str = "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}";

    /// The rendered base alone, which every row assertion below reads.
    fn binary_sign_asset_base(
        ctx: &Context,
        cfg: &SignConfig,
        binary: &Artifact,
        target: &str,
    ) -> anyhow::Result<String> {
        Ok(binary_sign_asset_naming(ctx, cfg, binary, target)?.base)
    }

    fn archive(id: &str, name_template: &str) -> ArchiveConfig {
        ArchiveConfig {
            id: Some(id.to_string()),
            name_template: Some(name_template.to_string()),
            ..Default::default()
        }
    }

    fn crate_with(archives: Vec<ArchiveConfig>) -> CrateConfig {
        CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            builds: Some(vec![BuildConfig {
                binary: Some("app".to_string()),
                targets: Some(vec![
                    LINUX.to_string(),
                    MUSL.to_string(),
                    WINDOWS.to_string(),
                ]),
                ..Default::default()
            }]),
            archives: ArchivesConfig::Configs(archives),
            ..Default::default()
        }
    }

    fn ctx_with(archives: Vec<ArchiveConfig>) -> Context {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .crates(vec![crate_with(archives)])
            .build();
        ctx.template_vars_mut().set("ProjectName", "app");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx
    }

    /// A binary whose own name differs from the project name, so a row that
    /// renders `{{ Binary }}` is told apart from one that renders
    /// `{{ ProjectName }}`.
    fn named_binary(target: &str, binary_name: &str) -> Artifact {
        let mut artifact = binary(target, None);
        artifact
            .metadata
            .insert("binary_name".to_string(), binary_name.to_string());
        artifact.name = binary_name.to_string();
        artifact.path = std::path::PathBuf::from(format!("target/{target}/release/{binary_name}"));
        artifact
    }

    fn binary(target: &str, id: Option<&str>) -> Artifact {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("binary_name".to_string(), "app".to_string());
        if let Some(id) = id {
            metadata.insert("id".to_string(), id.to_string());
        }
        Artifact {
            kind: ArtifactKind::Binary,
            name: "app".to_string(),
            path: std::path::PathBuf::from(format!("target/{target}/release/app")),
            target: Some(target.to_string()),
            crate_name: "app".to_string(),
            metadata,
            size: None,
        }
    }

    /// The already-registered archive of a publish-only run's preserved
    /// manifest. The base must not depend on it.
    fn add_archive(ctx: &mut Context, id: &str, stem: &str, file: &str) {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: stem.to_string(),
            path: std::path::PathBuf::from(format!("dist/{file}")),
            target: Some(LINUX.to_string()),
            crate_name: "app".to_string(),
            metadata: [("id", id), ("name", stem)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            size: None,
        });
    }

    /// The primary entry is the first in CONFIG order, and its template is
    /// what names the signature — whatever the registry holds, and whatever
    /// order it holds it in. `anodizer build` (empty registry) therefore
    /// names the asset exactly as `anodizer release` does.
    #[test]
    fn the_primary_entry_template_names_the_base_whatever_the_registry_holds() {
        let archives = vec![
            archive("default", TEMPLATE),
            archive(
                "extra",
                concat!(
                    "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}",
                    "-extra"
                ),
            ),
        ];
        let empty = ctx_with(archives.clone());

        let mut extra_only = ctx_with(archives.clone());
        add_archive(
            &mut extra_only,
            "extra",
            "app-1.0.0-linux-amd64-extra",
            "app-1.0.0-linux-amd64-extra.tar.xz",
        );

        let mut extra_first = ctx_with(archives.clone());
        add_archive(
            &mut extra_first,
            "extra",
            "app-1.0.0-linux-amd64-extra",
            "app-1.0.0-linux-amd64-extra.tar.xz",
        );
        add_archive(
            &mut extra_first,
            "default",
            "app-1.0.0-linux-amd64",
            "app-1.0.0-linux-amd64.tar.gz",
        );

        let mut default_first = ctx_with(archives);
        add_archive(
            &mut default_first,
            "default",
            "app-1.0.0-linux-amd64",
            "app-1.0.0-linux-amd64.tar.gz",
        );
        add_archive(
            &mut default_first,
            "extra",
            "app-1.0.0-linux-amd64-extra",
            "app-1.0.0-linux-amd64-extra.tar.xz",
        );

        for (label, ctx) in [
            ("no archive registered (anodizer build)", &empty),
            ("only the -extra archive registered", &extra_only),
            ("-extra registered first", &extra_first),
            ("default registered first", &default_first),
        ] {
            assert_eq!(
                binary_sign_asset_base(ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                    .unwrap(),
                "app-1.0.0-linux-amd64",
                "{label}"
            );
        }
    }

    /// The suffix the `signature:` / `certificate:` template appended to the
    /// binary's own file name carries onto the base — including the Windows
    /// `.exe` the raw binary carries and the base does not.
    #[test]
    fn the_suffix_after_the_binary_name_carries_onto_the_base() {
        let ctx = ctx_with(vec![archive("default", TEMPLATE)]);
        let base = binary_sign_asset_base(
            &ctx,
            &SignConfig::default(),
            &binary(WINDOWS, None),
            WINDOWS,
        )
        .unwrap();
        assert_eq!(base, "app-1.0.0-windows-amd64");
        for (rendered, binary_file, expected) in [
            ("app.sig", "app", "app-1.0.0-windows-amd64.sig"),
            ("app.exe.sig", "app.exe", "app-1.0.0-windows-amd64.sig"),
            (
                "app.bundle.sig",
                "app",
                "app-1.0.0-windows-amd64.bundle.sig",
            ),
            ("app.pem", "app", "app-1.0.0-windows-amd64.pem"),
        ] {
            assert_eq!(
                binary_sign_asset_name(rendered, binary_file, &base, WINDOWS),
                (expected.to_string(), AssetNameSource::Base),
                "{rendered} over {binary_file}"
            );
        }
    }

    /// A `signature:` template that RENAMED the file rather than suffixing the
    /// binary's own name has no suffix to carry, so the rendered basename is
    /// qualified with the triple instead — still one asset per target.
    #[test]
    fn a_renamed_signature_falls_back_to_the_target_qualified_name() {
        assert_eq!(
            binary_sign_asset_name("detached.sig", "app", "app-1.0.0-windows-amd64", WINDOWS),
            (
                format!("detached-{WINDOWS}.sig"),
                AssetNameSource::OutputTemplate
            )
        );
    }

    /// A target no archive entry covers keeps the full triple: `Os`/`Arch`
    /// render identically for the gnu and musl builds of one machine.
    #[test]
    fn an_uncovered_target_is_named_with_the_whole_triple() {
        let ctx = ctx_with(vec![ArchiveConfig {
            ids: Some(vec!["gnu".to_string()]),
            name_template: Some(TEMPLATE.to_string()),
            ..Default::default()
        }]);
        let base = binary_sign_asset_base(
            &ctx,
            &SignConfig::default(),
            &binary(MUSL, Some("musl")),
            MUSL,
        )
        .unwrap();
        assert_eq!(base, format!("app-1.0.0-{MUSL}"));
        assert_eq!(
            binary_sign_asset_name("app.sig", "app", &base, MUSL).0,
            format!("app-1.0.0-{MUSL}.sig")
        );
    }

    /// A `formats: [binary]` entry publishes the executable itself, so the
    /// signature is named after that asset — a `signs:` signature over the
    /// uploaded binary and a `binary_signs:` signature over the same bytes
    /// resolve to one name. The Windows `.exe` is part of it.
    #[test]
    fn a_binary_format_entry_names_the_base_after_the_published_executable() {
        let ctx = ctx_with(vec![ArchiveConfig {
            formats: Some(vec!["binary".to_string()]),
            ..Default::default()
        }]);
        assert_eq!(
            binary_sign_asset_base(&ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                .unwrap(),
            "app_1.0.0_linux_amd64"
        );
        assert_eq!(
            binary_sign_asset_base(
                &ctx,
                &SignConfig::default(),
                &binary(WINDOWS, None),
                WINDOWS
            )
            .unwrap(),
            "app_1.0.0_windows_amd64.exe"
        );
    }

    /// One archive carries a whole group of binaries under a single name, so
    /// naming both binaries' signatures after it uploads two assets called the
    /// same thing and the release keeps one. Each falls back to the whole
    /// triple instead.
    #[test]
    fn two_binaries_of_one_crate_register_distinct_signature_assets() {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .crates(vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                builds: Some(vec![
                    BuildConfig {
                        binary: Some("app".to_string()),
                        targets: Some(vec![LINUX.to_string()]),
                        ..Default::default()
                    },
                    BuildConfig {
                        binary: Some("helper".to_string()),
                        targets: Some(vec![LINUX.to_string()]),
                        ..Default::default()
                    },
                ]),
                archives: ArchivesConfig::Configs(vec![archive("default", TEMPLATE)]),
                ..Default::default()
            }])
            .build();
        ctx.template_vars_mut().set("ProjectName", "app");
        ctx.template_vars_mut().set("Version", "1.0.0");

        let base = |name: &str, amd64_variant: Option<&str>| {
            let mut artifact = binary(LINUX, None);
            artifact.name = name.to_string();
            artifact.path = std::path::PathBuf::from(format!("target/{LINUX}/release/{name}"));
            if let Some(variant) = amd64_variant {
                artifact
                    .metadata
                    .insert("amd64_variant".to_string(), variant.to_string());
            }
            binary_sign_asset_base(&ctx, &SignConfig::default(), &artifact, LINUX).unwrap()
        };
        assert_eq!(base("app", None), format!("app-1.0.0-{LINUX}"));
        assert_eq!(base("helper", None), format!("helper-1.0.0-{LINUX}"));
        assert_ne!(base("app", None), base("helper", None));

        // The build stage emits one artifact per micro-architecture level on
        // ONE target, so the triple alone does not separate them.
        assert_eq!(base("app", Some("v1")), format!("app-1.0.0-{LINUX}"));
        assert_eq!(base("app", Some("v3")), format!("app-1.0.0-{LINUX}v3"));
        assert_ne!(base("app", Some("v1")), base("app", Some("v3")));
    }

    /// The uncovered-target fallback appends the same amd64 clause every
    /// default name template does, so the two cannot drift apart.
    #[test]
    fn the_uncovered_target_template_ends_with_the_shared_amd64_suffix() {
        assert!(
            super::UNCOVERED_TARGET_NAME_TEMPLATE
                .ends_with(anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX),
            "uncovered-target template must reuse the shared amd64 variant suffix: {}",
            super::UNCOVERED_TARGET_NAME_TEMPLATE
        );
    }

    /// A `binaries:` allow-list that packs this binary alone keeps the entry's
    /// own name: the group is unambiguous again.
    #[test]
    fn a_binaries_filter_that_excludes_this_binary_moves_the_primary_to_the_next_entry() {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .crates(vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                builds: Some(vec![
                    BuildConfig {
                        binary: Some("app".to_string()),
                        targets: Some(vec![LINUX.to_string()]),
                        ..Default::default()
                    },
                    BuildConfig {
                        binary: Some("helper".to_string()),
                        targets: Some(vec![LINUX.to_string()]),
                        ..Default::default()
                    },
                ]),
                archives: ArchivesConfig::Configs(vec![
                    ArchiveConfig {
                        id: Some("app-only".to_string()),
                        binaries: Some(vec!["app".to_string()]),
                        name_template: Some(TEMPLATE.to_string()),
                        ..Default::default()
                    },
                    ArchiveConfig {
                        binaries: Some(vec!["helper".to_string()]),
                        ..archive(
                            "rest",
                            "{{ Binary }}-{{ Version }}-{{ Os }}-{{ Arch }}-rest",
                        )
                    },
                ]),
                ..Default::default()
            }])
            .build();
        ctx.template_vars_mut().set("ProjectName", "app");
        ctx.template_vars_mut().set("Version", "1.0.0");

        let base = |name: &str| {
            let mut artifact = binary(LINUX, None);
            artifact.name = name.to_string();
            artifact.path = std::path::PathBuf::from(format!("target/{LINUX}/release/{name}"));
            binary_sign_asset_base(&ctx, &SignConfig::default(), &artifact, LINUX).unwrap()
        };
        assert_eq!(base("app"), "app-1.0.0-linux-amd64");
        assert_eq!(base("helper"), "helper-1.0.0-linux-amd64-rest");
    }

    /// A meta entry packs no binaries at all, so the archive stage renders its
    /// name with an empty `{{ Binary }}`. The primary is the first entry that
    /// actually holds this binary.
    #[test]
    fn a_meta_first_entry_is_skipped_and_the_next_entry_names_the_base() {
        let ctx = ctx_with(vec![
            ArchiveConfig {
                id: Some("docs".to_string()),
                meta: Some(true),
                name_template: Some("{{ ProjectName }}-docs".to_string()),
                ..Default::default()
            },
            archive("default", TEMPLATE),
        ]);
        assert_eq!(
            binary_sign_asset_base(&ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                .unwrap(),
            "app-1.0.0-linux-amd64"
        );
    }

    /// The entry's `if:` is not evaluated: it can read the environment, and a
    /// signature named one way on the build host and another on the publish
    /// host is a release asset the verify gate cannot find.
    #[test]
    fn a_gated_primary_entry_still_names_the_base() {
        let ctx = ctx_with(vec![
            ArchiveConfig {
                if_condition: Some("false".to_string()),
                ..archive("default", TEMPLATE)
            },
            archive(
                "extra",
                "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}-extra",
            ),
        ]);
        assert_eq!(
            binary_sign_asset_base(&ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                .unwrap(),
            "app-1.0.0-linux-amd64"
        );
    }

    /// A sibling crate that configures archives but builds nothing counts
    /// toward the archive stage's work list only once something archivable is
    /// registered for it — which an `anodizer build` run has not done yet.
    /// Basing the multi-crate decision on that would rebind `ProjectName` on
    /// one command and not the other. What this holds is the `ProjectName`
    /// rebinding alone: the multi-crate default template renders the same
    /// string as the single-crate one, so the template half of the decision
    /// is held by the structural pin in `tests.rs` instead.
    #[test]
    fn the_base_is_stable_when_an_artifact_only_sibling_crate_has_nothing_registered_yet() {
        let run = |register_sibling_archive: bool| {
            let mut ctx = TestContextBuilder::new()
                .project_name("proj")
                .crates(vec![
                    crate_with(vec![ArchiveConfig::default()]),
                    CrateConfig {
                        name: "extras".to_string(),
                        path: "extras".to_string(),
                        archives: ArchivesConfig::Configs(vec![ArchiveConfig::default()]),
                        ..Default::default()
                    },
                ])
                .build();
            ctx.template_vars_mut().set("ProjectName", "proj");
            ctx.template_vars_mut().set("Version", "1.0.0");
            if register_sibling_archive {
                ctx.artifacts.add(Artifact {
                    kind: ArtifactKind::Archive,
                    name: "extras-1.0.0".to_string(),
                    path: std::path::PathBuf::from("dist/extras-1.0.0.tar.gz"),
                    target: Some(LINUX.to_string()),
                    crate_name: "extras".to_string(),
                    metadata: Default::default(),
                    size: None,
                });
            }
            binary_sign_asset_base(&ctx, &SignConfig::default(), &binary(LINUX, None), LINUX)
                .unwrap()
        };
        // What differs between the two runs is the `ProjectName` rebinding:
        // the registry-aware answer counts two crates once the sibling's
        // archive is registered and renders `app_…`. The template CHOICE is
        // not covered here — `DEFAULT_NAME_TEMPLATE_MULTI_CRATE` is defined
        // as `DEFAULT_NAME_TEMPLATE`, so both arms render alike; the
        // structural assertion in `tests.rs` holds that half.
        assert_eq!(run(false), "proj_1.0.0_linux_amd64");
        assert_eq!(run(true), run(false));
    }

    /// A lockstep multi-crate run rebinds `ProjectName` to each crate's own
    /// name while the archive stage renders that crate's asset, so the
    /// signature bases must differ by crate.
    #[test]
    fn a_lockstep_multi_crate_run_names_each_crate_signature_under_its_own_project_name() {
        let crate_cfg = |name: &str| CrateConfig {
            name: name.to_string(),
            path: name.to_string(),
            builds: Some(vec![BuildConfig {
                binary: Some(name.to_string()),
                targets: Some(vec![LINUX.to_string()]),
                ..Default::default()
            }]),
            archives: ArchivesConfig::Configs(vec![archive("default", TEMPLATE)]),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("proj")
            .crates(vec![crate_cfg("app"), crate_cfg("helper")])
            .build();
        ctx.template_vars_mut().set("ProjectName", "proj");
        ctx.template_vars_mut().set("Version", "1.0.0");

        let base = |name: &str| {
            let mut artifact = binary(LINUX, None);
            artifact.name = name.to_string();
            artifact.crate_name = name.to_string();
            artifact.path = std::path::PathBuf::from(format!("target/{LINUX}/release/{name}"));
            binary_sign_asset_base(&ctx, &SignConfig::default(), &artifact, LINUX).unwrap()
        };
        assert_eq!(base("app"), "app-1.0.0-linux-amd64");
        assert_eq!(base("helper"), "helper-1.0.0-linux-amd64");
    }

    /// A v3-tuned group's archive carries the micro-architecture level in its
    /// name, so the signature over its binary carries the same suffix.
    #[test]
    fn a_v3_group_signature_base_carries_the_amd64_suffix() {
        let ctx = ctx_with(vec![archive(
            "default",
            concat!(
                "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}",
                "{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
            ),
        )]);
        let mut artifact = binary(LINUX, None);
        artifact
            .metadata
            .insert("amd64_variant".to_string(), "v3".to_string());
        assert_eq!(
            binary_sign_asset_base(&ctx, &SignConfig::default(), &artifact, LINUX).unwrap(),
            "app-1.0.0-linux-amd64v3"
        );
    }

    /// `defaults.archives.format_overrides` applies to an entry that declares
    /// none of its own — exactly as the archive stage plans its outputs — so a
    /// global override to `binary` names the signature after the published
    /// executable.
    #[test]
    fn a_global_format_override_to_binary_names_the_signature_after_the_executable() {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .crates(vec![crate_with(vec![ArchiveConfig::default()])])
            .defaults(anodizer_core::config::Defaults {
                archives: Some(ArchiveConfig {
                    format_overrides: Some(vec![anodizer_core::config::FormatOverride {
                        os: "windows".to_string(),
                        formats: Some(vec!["binary".to_string()]),
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .build();
        ctx.template_vars_mut().set("ProjectName", "app");
        ctx.template_vars_mut().set("Version", "1.0.0");
        // The binary is named apart from the project, so the assertion holds
        // the template SHAPE — the archive default would render `app_…` — as
        // well as the `.exe` the executable's own asset name carries.
        assert_eq!(
            binary_sign_asset_base(
                &ctx,
                &SignConfig::default(),
                &named_binary(WINDOWS, "helper"),
                WINDOWS
            )
            .unwrap(),
            "helper_1.0.0_windows_amd64.exe"
        );
    }

    /// An entry producing several formats at once publishes the executable
    /// itself whenever `binary` is among them, whatever position it holds in
    /// the list.
    #[test]
    fn an_entry_listing_binary_second_still_names_the_signature_after_the_executable() {
        let ctx = ctx_with(vec![ArchiveConfig {
            formats: Some(vec!["tar.gz".to_string(), "binary".to_string()]),
            ..Default::default()
        }]);
        assert_eq!(
            binary_sign_asset_base(
                &ctx,
                &SignConfig::default(),
                &named_binary(WINDOWS, "helper"),
                WINDOWS
            )
            .unwrap(),
            "helper_1.0.0_windows_amd64.exe"
        );
    }

    /// `asset_name_template:` overrides every derived row, and renders in the
    /// same per-target scope.
    #[test]
    fn the_asset_name_template_override_wins() {
        let ctx = ctx_with(vec![archive("default", TEMPLATE)]);
        let cfg = SignConfig {
            asset_name_template: Some("{{ Binary }}-{{ Target }}-signed".to_string()),
            ..Default::default()
        };
        let base = binary_sign_asset_base(&ctx, &cfg, &binary(LINUX, None), LINUX).unwrap();
        assert_eq!(base, format!("app-{LINUX}-signed"));
        assert_eq!(
            binary_sign_asset_name("app.sig", "app", &base, LINUX).0,
            format!("app-{LINUX}-signed.sig")
        );
    }
}
