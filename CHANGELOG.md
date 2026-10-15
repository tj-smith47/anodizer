# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.30.0] - 2026-10-15

### Features

* 096b0fd2dda7 gate squash-merge expansion behind changelog.expand_squash_merges, off by default like GoReleaser ([@tj-smith47](https://github.com/tj-smith47))
* 30dcfc16f778 read a squash-merged pull request as the commits it squashed, for the changelog and the auto-bump ([@tj-smith47](https://github.com/tj-smith47))
* 1d2667d89714 accept every buildx annotation scope (index, manifest, index-descriptor, manifest-descriptor, platform-qualified, comma-joined) and default an unscoped key to index on a multi-platform build, resolve the base image through the rendered build args so a build arg overriding a global ARG names the real base in the OCI base annotations, and document manifest push_flags and the all-empty manifest skip ([@tj-smith47](https://github.com/tj-smith47))
* 43d71c985b2c add release.preflight.fail_on_error (default true, bool or template, per crate or at the root) so a failed GitHub push-access probe can warn and continue instead of stopping the run, and print publisher preflight warnings in human mode even when no one-way-door publisher is configured ([@tj-smith47](https://github.com/tj-smith47))
* afa38dab5c87 infer a release repository's owner and name from the origin remote when the block names only a token, for GitHub, GitLab and Gitea ([@tj-smith47](https://github.com/tj-smith47))
* 30e7d4ca5628 name the opened pull request's URL and every signed image in the GitHub Actions job summary ([@tj-smith47](https://github.com/tj-smith47))
* e0c0cca2c392 write a GitHub Actions job summary of what the run published when GITHUB_STEP_SUMMARY is set ([@tj-smith47](https://github.com/tj-smith47))
* c3e397e3cfb7 open the pull request with pull_request.token, falling back to repository.token then the token ladder, on the API and gh transports ([@tj-smith47](https://github.com/tj-smith47))
* 132cc2de70fb publish additional_locales as one locale manifest per entry, inheriting every unset field from the default locale ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 697ca9ecea82 name the JSON path of a Slack block or attachment string that fails to render, read the secret lint over the same string tree, and document every announcer's enabled field as a template ([@tj-smith47](https://github.com/tj-smith47))
* 1d365c20a283 render each string of a Slack block or attachment on its own so a literal quote and a rendered newline reach Slack as written ([@tj-smith47](https://github.com/tj-smith47))
* 97d8a2769c5f alias every PKGBUILD source per architecture, name the install script in .SRCINFO, and install from the archive's wrapping directory with a per-arch case when layouts differ ([@tj-smith47](https://github.com/tj-smith47))
* 662a4c305d5a name a PKGBUILD source alias from the archive's recorded format, so a url_template without an extension still aliases to the real tar.zst or zip name ([@tj-smith47](https://github.com/tj-smith47))
* 3a9add9f0031 build the source package from the uploaded source archive with its checksum, skip the entry when none exists, and honor arches ([@tj-smith47](https://github.com/tj-smith47))
* d6dcc71ca05a register the source archive under the crate whose release ships it, on dry runs too, and render the configured prefix_template when an older artifacts.json carries no prefix ([@tj-smith47](https://github.com/tj-smith47))
* a30bf23765e7 count only the build entries the stage would compile when refusing --output, so a skip: entry or one with no targets does not make a single build read as two, and hold the refusal quoted in the single-target docs to the message the command produces ([@tj-smith47](https://github.com/tj-smith47))
* 035d040d2737 decide the --output refusal on the run's own variables, so a build skipped by a rendered template such as IsSnapshot is not counted as a second binary, and say the srpm pretrans and posttrans paths are sourced on the installing host ([@tj-smith47](https://github.com/tj-smith47))
* 8bf645686482 name a duplicate arm64 darwin slice without an amd64 level in the universal binary refusal, since only an x86_64 slice has one ([@tj-smith47](https://github.com/tj-smith47))
* 7ee80e02c061 name each duplicate darwin slice's build id and amd64 level in the universal binary refusal, and ask for distinct builds[].id values that universal_binaries[].ids can select between ([@tj-smith47](https://github.com/tj-smith47))
* f2979d4af63e refuse build --output before anything compiles when the selection would produce more than one binary, naming each crate and the builds[] count ([@tj-smith47](https://github.com/tj-smith47))
* da8fa5ac3562 refuse two binaries on one darwin target when merging a universal binary, naming both paths, instead of merging whichever lipo saw first ([@tj-smith47](https://github.com/tj-smith47))
* 754033917564 expand squash merges from the GitHub, GitLab and Gitea sources too, list the squashed title first and drop its numbered duplicate ([@tj-smith47](https://github.com/tj-smith47))
* 5b33708beabc keep a touched file's leading and trailing spaces and its non-ASCII name, and keep a commit with no message, when reading commit records ([@tj-smith47](https://github.com/tj-smith47))
* 70e32d2c1867 read commit records with NUL separators so a message holding a record or field marker is no longer dropped or misread ([@tj-smith47](https://github.com/tj-smith47))
* 3dda397e722c build each git test fixture once per test binary and copy it, and run each case of the two slowest homebrew cask tests as its own test, so the Windows test job no longer times out ([@tj-smith47](https://github.com/tj-smith47))
* 4ab1cf6c26bf give each publisher and changelog fixture its own git repository, stop git at the workspace root under cargo so a test can no longer read the checkout's tags, gate the stage-sign and stage-source test helpers to match their callers so the Windows test build compiles, and replace grep -P and a GNU sed label loop in the coverage and gate-mirror scripts with awk so they run on macOS ([@tj-smith47](https://github.com/tj-smith47))
* 076807d51fc5 stop the macOS brew style check from misreading a coloured RuboCop summary, and keep the Windows token-file test off the network ([@tj-smith47](https://github.com/tj-smith47))
* b8e15e2877ad write docs page listings with forward slashes on Windows, keep a host gh.exe from answering for a gh stub, run the npm staged-conflict test on unix only, and update Homebrew before linting the rendered casks ([@tj-smith47](https://github.com/tj-smith47))
* 947fa98615ce build every missing-token hint from the env_files config so it names the configured token file, the default file when none is set, or the listed env files, read a GITHUB_TOKEN, GH_TOKEN, GITLAB_TOKEN or GITEA_TOKEN line in a listed env file as the run token when the variable is unset or blank, let a release block pick gitlab or gitea only when no forge token is set, clear GH_TOKEN when a non-GitHub forge is forced, name the forge in a missing-token error on a gitlab or gitea release, prove file-only, variable-beats-file and env-file tokens on all three forges end to end, catch an array pattern, an @ binding, a deferred let, a counted or defaulted read and a concat! name in the structural scans, follow a corpus split into a helper, make the coverage gate refuse the old COVERAGE_FLOOR name, state what 0.28.1 prints for a token source sentinel, and remove the remaining contrast comments ([@tj-smith47](https://github.com/tj-smith47))
* cba26cd65a22 read a GitHub token held only in the token file as the run token, after --token and the ANODIZER_GITHUB_TOKEN, GITHUB_TOKEN and GH_TOKEN variables, so a release with no token in the environment no longer stops at the token check, name the token file in every missing-token hint for GitHub, GitLab and Gitea from one helper, write the homebrew-core token source in the field v0.28.1 reads so an older binary still decodes the report, say unset or blank in the force-token docs, add GH_TOKEN to the forge detection doc, mark a helper parameter as a credential only when a caller passes one, exempt a forwarded credential only under its own name on a command, catch a let-else, tuple arm, struct pattern, stored closure, match, context or Some-empty comparison in the structural scans, read one shared corpus in every structural scan with a guard against a private split, require a known set of credential readers in the corpus, and replace floor and surface wording across comments, docs, printed strings and identifiers with plain English ([@tj-smith47](https://github.com/tj-smith47))
* 88b6a3cbac60 read a token line in a listed env file only for the forge the run has already resolved, after --token, the forge variables and the token file, so a GITLAB_TOKEN or GITEA_TOKEN line in a shared .env no longer moves a GitHub release to another forge and never counts in the multiple-token check, read that line as the last source in check config and the winget probe too, and give release.provider the full seven-step forge order with the release-block step and the env-file line ([@tj-smith47](https://github.com/tj-smith47))
* a57297466e30 load an unquoted 1 or 0 as a boolean in prerelease, make_latest, skip_push and wrap_in_directory as well, read 1 and 0 strings as booleans where the field is boolean-or-template, treat only a 429 as a forge rate limit in the GitLab and Gitea release preflight probes because neither forge marks a 401 or 403 as a throttle, print the GitLab job-token skip once per project at default output, and count only a whole-word release_preflight caller in the one-engine test ([@tj-smith47](https://github.com/tj-smith47))
* f6df93e1943b publish every duration field in the schema as a duration string, name JSON shapes and plain words only in the configuration reference, link a shared struct to the section of the field that holds it, render a const as its literal, read the cosign --tlog-upload value with Go's bool spellings, load a quoted auto, true, 1, false or 0 on prerelease, make_latest and skip_push in any letter case, key the bool-comparison allow-list by its exact line, attach the mcp command to the CLI tree that the reference, man page and bare help read, stop check config warning on changelog.use gitlab or gitea, and bring every docs page, README and field doc current with the code: preflight release checks and strict mode, bool-or-template load rules, retry and 429 waits, owner and name read from the origin remote, docker flag order, srpm fields, SBOM examples that load, package file names, cask custom_block contract, publisher token render failures and job summary lines ([@tj-smith47](https://github.com/tj-smith47))
* 7b6cd3cd1c55 read a rendered bool-or-template value as true on true or 1 and false on false, 0 or empty, trimmed and in any letter case, at every site that reads one, load an unquoted 1 or 0 as a boolean in prerelease, make_latest, skip_push and wrap_in_directory, accept archives: false in the published schema, name JSON shapes instead of Rust type names and escape the pipe in a union type in the configuration reference, document the accepted spellings in one table on the templates page, and hold every reader, every bool-or-template enum, the schema and the reference to that table with tests ([@tj-smith47](https://github.com/tj-smith47))
* 728bb93134d4 refuse a duration past the longest Duration with its value and bound the schema pattern to match, read --tlog-upload through one reader that takes only =value, the last occurrence and nothing after --, in the verify leg and the harness hardener alike, trim a pull-request or push token before deciding it is blank, run post: beside an empty hooks: list, list every selectable attestation kind from one place in core and name flatpak, install_script and file in the docs, report a comparison line copied into an allowed function, catch std type names in a reference description, and correct the documented behaviour of notarize skip and enabled, cask skips with no macOS artifact, build id defaults, nightly retention with keep_single_release, release.provider with force_token, every defaults.* merge rule, podman on macOS and Windows, the per-publisher retry deadline, livecheck with regex alone, strip_parent in file objects, submitter required, and 42 doc examples wrapped in the crates shape that loads ([@tj-smith47](https://github.com/tj-smith47))
* 3ebcc8c7804d read SOURCE_DATE_EPOCH first in a snapshot and fall back to the HEAD commit time with no dirty-tree offset, so a docker created label, a reproducible archive mtime and a PyPI mtime never carry a future date, let a publisher's own token variable win over the run token and never send a GitLab or Gitea token to GitHub, read WINGET_PKGS_TOKEN in the winget preflight, fall a blank chocolatey api_key through to CHOCOLATEY_API_KEY, fail validate-readme on a block that loads with a deprecation warning, replace the deprecated publish.homebrew example in the README with homebrew_cask, document the architecture names map_target produces with a test that reads the table, and state the real token order on every publisher page ([@tj-smith47](https://github.com/tj-smith47))
* 57cbbce360d4 update quinn-proto to 0.11.19 and rustls to 0.23.46 for RUSTSEC-2026-0338 through 0342 and 0346 through 0349 ([@tj-smith47](https://github.com/tj-smith47))
* 3615c6ddda73 cut an annotation key at its first = before reading its scope, keep the last of two build args rendering one key and drop a blank key or value, render build args before the empty-images skip, nest every docker docs example under crates, and hold the quoted manifest skip, push command and annotation lines with tests ([@tj-smith47](https://github.com/tj-smith47))
* a6e48e17807c keep --build-arg, --label and --annotation pairs in their template-key order, fail the docker docs release test when git or cargo is missing instead of passing silently, and fail the docker docs config test on any fence tag other than yaml, text or dockerfile ([@tj-smith47](https://github.com/tj-smith47))
* 7170c00be23a match a platform's binary by its amd64 level and read linux/arm64/v8 as arm64, so linux/amd64/v3 stages the v3 build and tags it amd64v3 ([@tj-smith47](https://github.com/tj-smith47))
* 71999d878317 pass user --label pairs after the sorted OCI labels in their template-key order with no final sort, hold the --build-arg, --label and --annotation order in the real build command, fail the docker docs config test when a page yields no yaml block, and read indented, four-backtick and tilde fences in both docker docs tests ([@tj-smith47](https://github.com/tj-smith47))
* d6ff67e8855a read a bare linux/arm platform as linux/arm/v7 the way buildx does, both for matching the armv7 build and for the staging directory, and name the earlier platforms[] entry first in the duplicate-directory refusal ([@tj-smith47](https://github.com/tj-smith47))
* 26e8872920f9 refuse two dockers_v2 platforms that stage under one TARGETPLATFORM directory in check config, and show the Dockerfile COPY selecting the binary by TARGETOS, TARGETARCH and TARGETVARIANT ([@tj-smith47](https://github.com/tj-smith47))
* adc5d97a96a5 stage linux/arm64/v8 under linux/arm64 as buildx spells TARGETPLATFORM, warn when a platform has no build to stage naming the amd64 level it needs, and refuse an amd64 level outside v1..v4 in check config ([@tj-smith47](https://github.com/tj-smith47))
* bc1d179bb34f count a working tree whose status cannot be read as dirty, so a corrupt index or a safe.directory refusal no longer passes the dirty-tree gate as clean ([@tj-smith47](https://github.com/tj-smith47))
* aaacee5c09c3 compare listing pages by their raw text so a server that ignores the page parameter ends the sweep after one repeat, and stop the sweep when the retry deadline passes ([@tj-smith47](https://github.com/tj-smith47))
* 222f807f4892 find an existing release through the by-tag endpoint and, where an older server has none, sweep every listing page instead of ten, so a repository with more than 500 releases no longer gets a second release for the tag ([@tj-smith47](https://github.com/tj-smith47))
* 017d1f7833bc stop the release listing sweep when the retry deadline passes or a server repeats a page, instead of looping forever on a server that ignores the page parameter ([@tj-smith47](https://github.com/tj-smith47))
* ac8358c10a68 page through every release link when replacing a duplicate, probe the server version once per run instead of once per asset, and keep the link create failure as the error a failed cleanup reports ([@tj-smith47](https://github.com/tj-smith47))
* ee7c3aa466f8 print a failed link cleanup once with the create failure first, probe the server version once across parallel uploads, and end a listing sweep on repeated link ids even when a counter field changed ([@tj-smith47](https://github.com/tj-smith47))
* ea039c35e60b retry the release link creation and the server version probe on a 5xx or 429, and report a 400 that no existing link explains as the create failure itself instead of posting the same link again ([@tj-smith47](https://github.com/tj-smith47))
* d3ceb60300a3 stop the release-link sweep when a page repeats or the deadline passes, cache the version probe only when it answered, and report a failed link cleanup with the create failure first ([@tj-smith47](https://github.com/tj-smith47))
* 354902dcd714 add depends_on :macos to a cask only when a macOS-only stanza has no Linux side, render a universal macOS artifact flat inside on_macos beside Linux blocks, and hold the cask docs examples to the renderer ([@tj-smith47](https://github.com/tj-smith47))
* 4d956394af43 carry a custom_block statement across a backslash continuation and a ?\ char literal, read a -> { } body as a block scope, and trim trailing whitespace only on a line that ends in code when reindenting a formula or cask block ([@tj-smith47](https://github.com/tj-smith47))
* 89a260aae3c4 clear the block-parameter position whenever a custom_block interpolation swaps the code buffer so a line ending in a comma after a closed |y| still reads on, and leave class, for and module out of the keywords that open a literal ([@tj-smith47](https://github.com/tj-smith47))
* 681469e244b6 fall back to the word scan and warn when a << ? or % after an untracked word opens a literal that spans lines, bind names from operator and multiple assignments, for, rescue, def, lambda and multi-line block parameters, cut comments from the word scan, and keep brackets inside code spans out of rustdoc link unwrapping ([@tj-smith47](https://github.com/tj-smith47))
* f20b1429e623 give every platform a formula download and refuse the ones not built for through Homebrew requirements, render def plist without the plist_options call Homebrew 7 dropped, skip archives Homebrew has no variant for, add a depends_on_macos cask override beside the whole-word custom_block scan, log each render once per command, and read publish.homebrew_cask on every path so a cask-only crate publishes ([@tj-smith47](https://github.com/tj-smith47))
* 8315946a8705 keep depends_on :macos on every cask without a Linux side, fill a missing macOS architecture from the universal artifact instead of dropping it, and read custom_block stanzas at the top level only ([@tj-smith47](https://github.com/tj-smith47))
* f819a53c7b05 keep each custom_block interpolation as its own code text so a ; inside one never splits the enclosing depends_on statement, read a depends_on and a local assignment inside an interpolation, read a ? against a method name as a character literal, and read a / after an operator as a regex ([@tj-smith47](https://github.com/tj-smith47))
* 52b4327ad788 keep the whitespace of a custom_block literal when reindenting by trimming a line only past its last literal, read a backslash at the end of an interpolating heredoc body line as joining the next line, and keep a carriage return inside a literal ([@tj-smith47](https://github.com/tj-smith47))
* d47e29908528 lex a custom_block so a string, percent literal or heredoc that spans lines, an interpolation with nested quotes, a ?x literal or a regex is set aside before the Linux-side and depends_on scans, and continue a statement after => or inside an open literal ([@tj-smith47](https://github.com/tj-smith47))
* 4b44349ad0b8 order formula dependencies by type then case-insensitive name, render every dependency type, and sort cask dependencies the way brew style requires ([@tj-smith47](https://github.com/tj-smith47))
* bdfc3c9b032c push a same-tap cask when only the formula's gate trips, print each gate skip once from the publisher, read a custom_block's depends_on as whole statements, name every archive in the no-installable-archive error, share a tap only on the same branch, token and pull request settings, and spell the run summary's entry count in plain words ([@tj-smith47](https://github.com/tj-smith47))
* b45f91270664 put OS-scoped dependencies inside the formula's existing on_macos or on_linux block instead of opening a second one ([@tj-smith47](https://github.com/tj-smith47))
* 4f951e2680a2 read << ? and % in a custom_block as a shift, a ternary or modulo after a value and as a heredoc, a char literal or a percent literal elsewhere, so a Linux side after such a line is still seen ([@tj-smith47](https://github.com/tj-smith47))
* 790786a72618 read a custom_block =begin comment only in code and keep an open interpolation across it, bind an operator-assignment target wherever it stands, bind no named group from an interpolated regex, read % and a backtick after a label or keyword and a ? before a quote as literals, read a : after a head call as a symbol, and read a name opening a continued hash or argument line as a label ([@tj-smith47](https://github.com/tj-smith47))
* 9bbaab6eefae read a custom_block =begin on a heredoc body line as body text, open a literal after every keyword that leaves an operand (begin, break, do, ensure, for, module, next, rescue), read a backtick after a label as a command string, and end a statement at a line ending in a name with ? or !, the bar closing block parameters, a closing bracket, a literal or a semicolon while an open interpolation or a ; or #{ on a continued line starts one ([@tj-smith47](https://github.com/tj-smith47))
* fe5c3811ab84 read a custom_block interpolation through the one code path so its scopes, sigils and symbols bind as outside a literal, read a word after a call operator as a method whatever it spells, bind <<= >>= and ^= targets, read an interpolation spanning heredoc body lines, and open a literal for certain after a name ending in ? or ! ([@tj-smith47](https://github.com/tj-smith47))
* 3754fcfa4f7d read a custom_block through one per-line code-text pass that sets strings, percent literals and trailing comments aside for both scans, continue a depends_on statement across open brackets, and key a top-level cask's skip_upload warning by its index so two entries sharing a name both warn ([@tj-smith47](https://github.com/tj-smith47))
* b4722487d099 read a custom_block's Linux side and macOS dependency at any depth, take a flat cask download from its single block so a universal DMG beside per-arch binaries renders, fill a missing architecture only from the same build, and log the universal skip once ([@tj-smith47](https://github.com/tj-smith47))
* a11c03080eb5 read a custom_block's depends_on statements with comments and strings set aside, share a tap only when the cask's whole repository block matches the formula's, print gate warnings once in every command including snapshot and dry-run, and spell every count in a log line for its number ([@tj-smith47](https://github.com/tj-smith47))
* 06b22db22da6 read a custom_block's locals, instance variables and keywords as values before << ? and %, and when a literal is still open at the end of the block fall back to a word scan and warn once that depends_on_macos decides it ([@tj-smith47](https://github.com/tj-smith47))
* d52cbc6f7c40 read a typed percent literal closed by its newline, join its content line on a trailing backslash, read interpolations inside a heredoc body, read an operator after . &. or :: as a method name and continue the statement when the call operator ends the line, and take the letters after a newline-closed %r as its options ([@tj-smith47](https://github.com/tj-smith47))
* 456799a978f6 read the word after a :: that ended an earlier line as a constant path, read an operator symbol as a value, and count the code inside an interpolation toward the Linux-side and depends_on checks ([@tj-smith47](https://github.com/tj-smith47))
* 553b65ddda04 reindent only the custom_block lines that begin in code so a plain heredoc closer stays at column 0, read a regex after a keyword or an untracked word, scope bound names to their block or method, bind nested destructuring, captures and rightward assignments, read every cask stanza as taking an operand, and scan comments and strings too when the block falls back to a word scan ([@tj-smith47](https://github.com/tj-smith47))
* c2ef2704d781 render casks in Homebrew's stanza order so they pass brew style, write service as a bundle path and drop formula conflicts that Homebrew refuses to load, and end check config with a verdict when deprecated properties were used ([@tj-smith47](https://github.com/tj-smith47))
* 018575921bde render formulae in Homebrew's component order with one top-level install so they pass brew style, read a custom_block's Linux side from its raw text so an unbalanced quote can never hide it, count every macOS cask download with one predicate, fill a missing cask architecture only when both ids match, and print the universal skip line once per run ([@tj-smith47](https://github.com/tj-smith47))
* 7eadb80808f9 render one on_macos and one on_linux block for all of an OS's dependencies instead of a block per dependency ([@tj-smith47](https://github.com/tj-smith47))
* abc08b074010 let a publisher's and an SBOM tool's cmd, args and dir read their own env entries, read changelog file names unquoted, and name the sbom extension through the one artifact accessor ([@tj-smith47](https://github.com/tj-smith47))
* 58c1fe3b37f9 render env entries in sequence so a later entry, and a hook's cmd and dir, can read the entries defined before them ([@tj-smith47](https://github.com/tj-smith47))
* e510be8f91a1 claim a package's output path before rendering its sidecars so a colliding job writes nothing, and drop an overrides map the lintian removal emptied ([@tj-smith47](https://github.com/tj-smith47))
* 3ecd981d24aa honour deb.lintian_overrides declared under overrides.<format> and refuse two jobs that render one package file name before any package is written ([@tj-smith47](https://github.com/tj-smith47))
* 5c776108cbcf record the package's conventional extension on its artifact so ArtifactExt renders .pkg.tar.zst for an Arch package ([@tj-smith47](https://github.com/tj-smith47))
* 8b924b8ef9d3 read npm's E409 'previously staged version' as an accepted publish instead of failing the re-run ([@tj-smith47](https://github.com/tj-smith47))
* 188dd25af10a fail the pull request when pull_request.token or repository.token cannot render instead of sending the literal template as the bearer ([@tj-smith47](https://github.com/tj-smith47))
* c9401bf1adf4 keep the homebrew-core token source when an older report is decoded and written again so a rollback names it and a merge keeps one target, list every token source the krew and schemastore rollbacks read in their missing-token warning from the shared order, name the GitHub token ladder once in a GitHub-run hint, read a blank ANODIZER_FORCE_TOKEN as unset so GORELEASER_FORCE_TOKEN is still read, follow a credential through a move, a tuple destructure, a helper argument or a stored closure and an environment credential through a const name, option_env, a pattern test or a suffix strip in the structural scans, scan static and const initializers, share one production corpus between every structural scan, and remove the remaining banner lines, audit tags and floor wording ([@tj-smith47](https://github.com/tj-smith47))
* 6b7782004d5e keep the template's text out of a pull request token render error, and resolve the token and krew's target before the clone ([@tj-smith47](https://github.com/tj-smith47))
* ae506ab3d6a6 name the run token in the homebrew-core token requirement and its missing-token messages in the order the resolver reads, so a run that has a token is not told to set HOMEBREW_CORE_GITHUB_TOKEN, read COSIGN_KEY and GPG_PRIVATE_KEY in the determinism harness through the trimming reader so a blank value no longer skips signing, catch a credential rendered through a guarded or ref match arm, a chain of bindings or a helper parameter and an environment credential read through var_os or a length or empty-string test in the structural scans, say unset or blank in every skip message for a blank credential, state that the authenticode password is trimmed, restore the podman, homebrew deprecation, notarize and podman-refusal facts the wording sweep dropped, correct the always() comment in the release pipeline docs, and remove every banner comment line in the workspace ([@tj-smith47](https://github.com/tj-smith47))
* c20d3140ca5b read one pull-request token order in homebrew-core too, so the run token on a GitHub run comes before ANODIZER_GITHUB_TOKEN, GITHUB_TOKEN and GH_TOKEN, derive every publisher page's token variables from the resolver calls in its code, render the aur rollback private_key as a credential, treat a blank Discord webhook_url or webhook endpoint_url as unset through one config-or-environment secret helper shared by announce and publish, trim the authenticode password, the npm rollback and promote tokens, the dockerhub rollback password, the MCP GitHub token, the artifactory rollback check and preflight's required variables so a padded value is read and a blank one counts as unset, catch a credential field rendered through a variable or a computed environment name in the structural scans, name a sign stdin as signs[id].stdin, state that stdin and every credential are trimmed, correct the post_publish_poll enabled default and the pull_request.token doc, link the duration digit cap to its constant, use the real clock for bluesky, email and x timestamps, and print dependency minimum in bump output ([@tj-smith47](https://github.com/tj-smith47))
* a4dfd438ec89 record where the homebrew-core bump token came from and name that source in the rollback warning, list the token variables in the homebrew-core requirement on a non-GitHub run even when --token is set, build every token-order hint from the shared order constants with the run token listed only on a GitHub run, keep the sign stage on a preserve run when COSIGN_KEY is blank and strip its ephemeral signatures before publish, follow a credential through a method call, a tuple or struct field and a map built from the environment in the structural scans and count a trim only when it comes before the presence test, bring the two scans from 60 and 31 seconds to 2 and under 1 by reading every source once, and remove the remaining audit tags, jargon and contrast frames from comments ([@tj-smith47](https://github.com/tj-smith47))
* a05295a543b1 render every credential field through one helper that trims, errors when the template fails and redacts the template text, so an srpm or nfpm key_passphrase, an upload or artifactory header, a git repository url, a notarize password, a sign stdin, an announce token or webhook url and an mcp or dockerhub header are never sent as raw template text, read every credential variable through one reader that treats a blank or padded value as unset, probe the pull-request token the publish opens with in the homebrew-core and schemastore preflights and the release token the stage sends in the release requirement, state the real push and pull-request token order on every publisher page from one shortcode and in the pull_request.token field doc, read the source date through one reader in tag, bump, promote, rollback, makeself and nsis so a malformed value is an error everywhere, state the one-year cap on all 11 duration fields, refuse a mod_timestamp or epoch no file time can hold instead of panicking, and fix the nine rustdoc link errors that failed the doc gate ([@tj-smith47](https://github.com/tj-smith47))
* 7b45c04b1133 render every secret field through one helper that errors when the template fails, so a chocolatey api_key or any token is never sent as raw template text, resolve a pull-request token through the same ladder as the push token with the publisher's own variable before the run token, probe the winget preflight with the pull-request token, send the run token from promote only after a forge check, close a krew rollback PR with the publish token and the run's own env, trim every token read from the environment, read the committer time for CommitTimestamp, CommitDate and the snapshot source date alike, read an ambient SOURCE_DATE_EPOCH first in a reproducible build and in every reader through one parser that names a malformed value, cap every duration at one year through one grammar shared with --timeout so a loaded config cannot panic, match only space and tab in the duration pattern so the schema and the parser agree under both regex dialects, default a cask livecheck to url :url, and correct the retry deadline, podman, chocolatey, upload and artifactory docs, 72 contrast frames and 30 headings ([@tj-smith47](https://github.com/tj-smith47))
* fe5571afde1a resolve pull_request.token before the tap push so a template that does not render skips the crate with nothing pushed, and keep krew's pushed branch on its Failed row ([@tj-smith47](https://github.com/tj-smith47))
* 5bfdb2c462c5 explain a GitHub asset upload the API redirects because the repository was renamed or transferred, instead of a bare 307 ([@tj-smith47](https://github.com/tj-smith47))
* f468a4dbdce5 fill a release repository's owner and name only from a remote on that forge's host, keep a namespace-less GitLab project id as written, and read every URL host through one parser ([@tj-smith47](https://github.com/tj-smith47))
* ba6d584e1e48 keep a test run's publish rows out of the CI job summary and count no custom publisher as run on a dry run ([@tj-smith47](https://github.com/tj-smith47))
* bd60fa116351 load an unquoted 0 or 1 as a boolean in every string-or-bool field, give the GitLab and Gitea preflight probes their own labels, headers and rate-limit reads so a forge rate limit warns instead of blocking, skip the GitLab access check on a CI job token because the Projects API refuses it, document that a probe with no verdict warns unless preflight.strict is set, hold every caller of release_preflight inside the one preflight engine, and prove an empty top-level fail_on_error falls back to the default ([@tj-smith47](https://github.com/tj-smith47))
* e8140529484d name a homebrew tap row in the job summary without calling a cask a formula, and report the open pull request's own address ([@tj-smith47](https://github.com/tj-smith47))
* 226c57ca71d3 name the reappearance in the error of an asset upload that exhausts its attempts behind an eventually consistent delete ([@tj-smith47](https://github.com/tj-smith47))
* 07c9f5ad4865 read a GitLab block's empty owner as a project id only for a numeric id or a full path, match a forge host whatever port the remote or the API base carries, and name the remote's host in the refusal only when it is another forge's ([@tj-smith47](https://github.com/tj-smith47))
* f6301bd6a4f4 read every scp-style remote through one parser, so a bare host:path remote and a bracketed IPv6 host resolve the same owner, name and host everywhere ([@tj-smith47](https://github.com/tj-smith47))
* fbddfc68c6b2 read release.preflight.fail_on_error strictly (true, false, 1 or 0, any other render stops the run, an empty crate value falls back to the root), report a template error beside the probe it decides on, probe GitLab developer access and Gitea push permission, fail the preflight when the tag's GitHub release is already immutable, route a missing token through the knob, name GITLAB_TOKEN or GITEA_TOKEN in the environment requirement on those forges, and prove anodizer release continues past a warning-only preflight ([@tj-smith47](https://github.com/tj-smith47))
* 771e48460961 read the status of every GitHub release and asset delete, fail an upload whose conflicting asset cannot be removed, and classify the re-upload like the first post ([@tj-smith47](https://github.com/tj-smith47))
* e1240a2f5baf read the status of the nightly retention tag delete, and delete a reappeared release asset again instead of keeping its stale bytes ([@tj-smith47](https://github.com/tj-smith47))
* 990f534f2e39 replace a conflicting GitHub release asset inside the attempt that met it, and pin draft reuse by rendered title and the token kept through owner/name inference ([@tj-smith47](https://github.com/tj-smith47))
* b6705cadce71 run the release repository inference in anodizer promote, refuse an unfilled owner or name before any URL is built, read an Enterprise remote when github_urls.api names it, and tolerate an unfilled block on every forge under a snapshot ([@tj-smith47](https://github.com/tj-smith47))
* e1dc3dcdf685 compare a listing page by its ids only when every entry has one, else by its text, and test the GitHub retry classifier directly instead of through a test-only copy ([@tj-smith47](https://github.com/tj-smith47))
* 800e146872f5 honor Retry-After and RateLimit-Reset on a 429, so a rate-limited HTTP call waits the time the server asked for instead of the policy's backoff step ([@tj-smith47](https://github.com/tj-smith47))
* 0b19efbd104e let every retry ladder honor the wait a 429 asks for, the announce clients included, and say 'exhausted retry attempts' only when a ladder was actually spent, never on a one-attempt fast fail ([@tj-smith47](https://github.com/tj-smith47))
* d83b600057c8 make PyPI, GemFury and Chocolatey honor the wait a 429 asks for, print each HTTP failure once instead of three times, and read every HTTP status through one chain walker ([@tj-smith47](https://github.com/tj-smith47))
* 5308270415c7 name the call once when a transport ladder is spent, and refuse any HttpError built from a status without the 429 wait wherever it is built ([@tj-smith47](https://github.com/tj-smith47))
* 209492696eaf capture a child's output per chunk so an unterminated last line read before the drain cut-off is kept, and name the drain constants in the module docs ([@tj-smith47](https://github.com/tj-smith47))
* cff082ce4fde keep the partial line read before a pipe error, state the drain window and its Windows stdin exception in the module docs, and unlink a private item from the remote_host rustdoc ([@tj-smith47](https://github.com/tj-smith47))
* cf66659a7d55 release a child's pipes within the drain window after it exits, so a reader or stdin writer blocked on a pipe a grandchild still holds no longer keeps the thread and descriptor for that process's life ([@tj-smith47](https://github.com/tj-smith47))
* 83522458c115 stop using a child's pipes five seconds after it exits when a process it started still holds them, so a backgrounded grandchild no longer blocks the run helpers for its whole life ([@tj-smith47](https://github.com/tj-smith47))
* 0b62de0ce16f upload a per-artifact SBOM with its subject's crate and expect the workspace SBOM on the release owner's release, so a lockstep workspace whose project_name names a non-owner crate passes verify-release ([@tj-smith47](https://github.com/tj-smith47))
* 52189c88e61c report a bad additional_locales entry on a dry run and quote the skip line a default run prints, and fix(sbom): expect no document for a directory-bundle installer ([@tj-smith47](https://github.com/tj-smith47))
* 187763395642 build every git spawn through one git_command constructor that sets GIT_TERMINAL_PROMPT=0, LC_ALL=C, column.ui=never and color.ui=never once, route the publisher runners through a run_git_in family, and fail the build when a raw git spawn appears anywhere else ([@tj-smith47](https://github.com/tj-smith47))

---
### Performance

* b21d5482eae3 scan only the newly read bytes for line breaks when teeing output, so a long single-line child output no longer costs quadratic time under -v ([@tj-smith47](https://github.com/tj-smith47))

## [0.29.0] - 2026-10-11

### Features

* b8e2ab43954b add the targetVariant function and the Abi variable to every per-target scope ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 8851f0a10557 a webhook header named in headers replaces the default of the same name ([@tj-smith47](https://github.com/tj-smith47))
* 08b91f182693 refuse two entries that render one package name ([@tj-smith47](https://github.com/tj-smith47))
* 87eff73d0b43 ignore a trailing slash on a remote URL and parse ssh:// remotes ([@tj-smith47](https://github.com/tj-smith47))
* 31cf2d6c3dbc escape newlines, carriage returns and tabs in the package description ([@tj-smith47](https://github.com/tj-smith47))
* ec094af3a81d refuse a shallow checkout that hides a tag of a planned family and consult every remote ([@tj-smith47](https://github.com/tj-smith47))
* c8f8703cb1b3 refuse a shallow checkout whose history does not reach the last tag ([@tj-smith47](https://github.com/tj-smith47))
* ead9d6bcdf16 keep a failed publisher's committed work on record so tag rollback withdraws it ([@tj-smith47](https://github.com/tj-smith47))
* d2118773d8af a #none marker vetoes the bump a conventional commit would infer ([@tj-smith47](https://github.com/tj-smith47))
* 063fdfd6e7a1 wait up to 12 minutes for a registry to serve a published artifact ([@tj-smith47](https://github.com/tj-smith47))
* 91d7b5510176 mask a job's env in every result line, retry line and dry-run echo, and drop a dry-run env entry that does not render ([@tj-smith47](https://github.com/tj-smith47))
* 01e160e23ee7 redact a job's own env from every subprocess's logged output and error text ([@tj-smith47](https://github.com/tj-smith47))

---
### Performance

* a903263429a3 stream uploads to the object store instead of reading each artifact into memory ([@tj-smith47](https://github.com/tj-smith47))

## [0.28.1] - 2026-09-15

### Bug Fixes

* ba7728eb415d package the binstall table in the reconcile probe so a re-run of the sha that published no longer reads as diverged ([@tj-smith47](https://github.com/tj-smith47))
* 39832413ef41 retry an hdiutil create that fails with "Resource busy" instead of failing the run ([@tj-smith47](https://github.com/tj-smith47))
* f8aa8e0a6f12 retry an npm publish whose OIDC identity token could not be fetched ([@tj-smith47](https://github.com/tj-smith47))

## [0.28.0] - 2026-09-14

### Features

* 80f1d42cc80c run the whole preflight once, before the tag, through one engine ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* c93b27f38dad ask each publisher whether its rollback credential is really missing, and keep a clean release free of by-design warnings ([@tj-smith47](https://github.com/tj-smith47))
* f3c7d491f956 keep the Windows test run honest about a .cmd stub and a forward-slash size report ([@tj-smith47](https://github.com/tj-smith47))
* f86fc7f428a3 lock the TUF store for keyed cosign, sign inside the harness, and account for every skipped or retried step ([@tj-smith47](https://github.com/tj-smith47))
* be9883bef4e3 package the whole cargo publish set in one dry-run so a pre-tag preflight verifies siblings locally ([@tj-smith47](https://github.com/tj-smith47))
* 256907bf781b probe the chocolatey feed's service document, since a GET on the push route can never succeed ([@tj-smith47](https://github.com/tj-smith47))

## [0.27.0] - 2026-09-13

### Features

* 2b8d6e48486b probe PyPI landing and share one propagation window across the whole sweep ([@tj-smith47](https://github.com/tj-smith47))
* e7961e390400 probe every pushed docker image tag for its landing ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 4def8ddba4a0 resolve every target's archive formats through one resolver ([@tj-smith47](https://github.com/tj-smith47))
* 40c0f923a588 warn that asset_name_template is ignored outside binary_signs ([@tj-smith47](https://github.com/tj-smith47))
* dfcb025ae2d2 check every sign slice for duplicate outputs and mis-padded placeholders, and stop warning when a placeholder renders a path ([@tj-smith47](https://github.com/tj-smith47))
* 906b8401b761 offer the digest spelling on a docker sign stdin ([@tj-smith47](https://github.com/tj-smith47))
* 665978486177 rewind the defaults provenance record between workspaces ([@tj-smith47](https://github.com/tj-smith47))
* d808c642de68 stop check config warning about two sign entries that select different artifacts ([@tj-smith47](https://github.com/tj-smith47))
* 3d3605660126 warn on a duplicate sign output that an empty if: or a ./ hides ([@tj-smith47](https://github.com/tj-smith47))
* 4132c2a75e71 warn on a duplicate sign output whose template renders outside dist ([@tj-smith47](https://github.com/tj-smith47))
* 2a0b355cdc21 warn on docker sign templates the docker path never expands ([@tj-smith47](https://github.com/tj-smith47))
* 217da5d0efa8 warn on every placeholder spelling a sign template can fail on ([@tj-smith47](https://github.com/tj-smith47))
* 857b944e7df9 warn on the docker sign literals check config missed ([@tj-smith47](https://github.com/tj-smith47))
* 61f01d8b0500 warn on two binary_signs entries writing one file only when both really write it ([@tj-smith47](https://github.com/tj-smith47))
* 6ff23434e7ec warn when an ungated binary_signs entry meets a gated one ([@tj-smith47](https://github.com/tj-smith47))
* 9c9cb0b95b82 warn when two binary_signs entries sign one file ([@tj-smith47](https://github.com/tj-smith47))
* e95c65fc1125 treat a blank `if:` as the no-op gate an empty one already is ([@tj-smith47](https://github.com/tj-smith47))
* 9dfd681d0ebe measure the podman digestfile and name 2.0 as the floor ([@tj-smith47](https://github.com/tj-smith47))
* 4b4590a6c018 record every image tag whose registry push succeeded ([@tj-smith47](https://github.com/tj-smith47))
* 696f5d5523fe record the pushed manifest digest from buildx metadata ([@tj-smith47](https://github.com/tj-smith47))
* 62df8f09aa04 record the pushed manifest digest under podman and always report the created images ([@tj-smith47](https://github.com/tj-smith47))
* daf88b7576bc report the first recorded podman digest and keep the buildx metadata as fixtures ([@tj-smith47](https://github.com/tj-smith47))
* ffbd717165a6 strip every npm_config credential variable, and report each landing result line in singular and plural ([@tj-smith47](https://github.com/tj-smith47))
* 440c24b10085 strip the registry-scoped username and password variables ([@tj-smith47](https://github.com/tj-smith47))
* d8db10592d29 grade an unusable npm token by what else can authenticate ([@tj-smith47](https://github.com/tj-smith47))
* d37829b29dd7 keep the parent `if:` gate when a cask or schema entry sets an empty one ([@tj-smith47](https://github.com/tj-smith47))
* b658d902f5db let an OIDC npm publish survive a dead NPM_TOKEN ([@tj-smith47](https://github.com/tj-smith47))
* cd60d7c0e154 never open a GitHub Discussion for a nightly release ([@tj-smith47](https://github.com/tj-smith47))
* f555ef13f3fb report the withheld nightly discussion category at default verbosity ([@tj-smith47](https://github.com/tj-smith47))
* 81dfb3cb6934 claim the certificate name and word the remedy per derivation ([@tj-smith47](https://github.com/tj-smith47))
* 3dd3396d6242 compare two signature paths with '..' folded away ([@tj-smith47](https://github.com/tj-smith47))
* 0c5e2fc9a522 derive binary signature asset names from config, one rule per target ([@tj-smith47](https://github.com/tj-smith47))
* 8b78cf2b0790 expand ${artifact} before joining dist, so the stage and the gate name one file ([@tj-smith47](https://github.com/tj-smith47))
* 6ea4255080e6 fail the run when two binaries resolve to one signature asset name ([@tj-smith47](https://github.com/tj-smith47))
* d00fffe37d0f keep a Windows verbatim path whole wherever a sign output is compared ([@tj-smith47](https://github.com/tj-smith47))
* e9f0daa7c10b key a signature asset name on the file it signs ([@tj-smith47](https://github.com/tj-smith47))
* a78d28980e10 key the asset-name collision claim on the binary's identity ([@tj-smith47](https://github.com/tj-smith47))
* 7fc444ceeab1 name a binary signature per micro-architecture level and from config alone ([@tj-smith47](https://github.com/tj-smith47))
* 9e8b8ffbf6d3 name binary signatures after the crate's primary archive entry ([@tj-smith47](https://github.com/tj-smith47))
* 910a3181b2f8 name each binary's signature per crate, target and binary ([@tj-smith47](https://github.com/tj-smith47))
* 1885560ac551 refuse one asset name over a binary's two outputs ([@tj-smith47](https://github.com/tj-smith47))
* 25275512a8a3 write a signature that hops out of dist to the folded path ([@tj-smith47](https://github.com/tj-smith47))
* ee3e25069a32 answer the registry's own challenge and digest ([@tj-smith47](https://github.com/tj-smith47))
* 947381f33019 bound every landing probe by the sweep's own window ([@tj-smith47](https://github.com/tj-smith47))
* c3edecbce857 name every landing host once and in one clause ([@tj-smith47](https://github.com/tj-smith47))
* ffe07ebc46d1 retry landing probes while a registry propagates a publish ([@tj-smith47](https://github.com/tj-smith47))
* 5a8d2610313b say "among the selected publishers" on the skip lines ([@tj-smith47](https://github.com/tj-smith47))
* 2e1c3148de21 name the selected publishers in the skip and help lines ([@tj-smith47](https://github.com/tj-smith47))
* 0273a2cf01da reword the rollback and publish-only messages an operator reads ([@tj-smith47](https://github.com/tj-smith47))
* eb4361d88764 move the asset-name cluster to its own module ([@tj-smith47](https://github.com/tj-smith47))

## [0.26.0] - 2026-09-12

### Features

* 5415072b93af document the env-var surface behind --help ([@tj-smith47](https://github.com/tj-smith47))
* a5ce85b0dc2f pick the matching libc at install time ([@tj-smith47](https://github.com/tj-smith47))
* 248a77130e2a serve the anodizer CLI to MCP clients ([@tj-smith47](https://github.com/tj-smith47))
* 51aa68dcaeab skip the release when the changelog range is empty ([@tj-smith47](https://github.com/tj-smith47))
* f082bfa8b3cf derive the Full Changelog link and default the release body footer ([@tj-smith47](https://github.com/tj-smith47))
* c63b574ae8a1 upload binary_signs output as release assets ([@tj-smith47](https://github.com/tj-smith47))
* 8abde07b3171 add the join path function with filepath.Join semantics ([@tj-smith47](https://github.com/tj-smith47))
* cdcfe2083ad8 scope an enrollment to one occurrence and rewrite shared entries with distinct old versions ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* b540ce4624d3 claim every binary output path before writing any ([@tj-smith47](https://github.com/tj-smith47))
* 4028c3068d85 claim every planned output across all crates and targets before the first copy ([@tj-smith47](https://github.com/tj-smith47))
* 075b8bd2e244 close a copied binary through the archive write-back helper ([@tj-smith47](https://github.com/tj-smith47))
* 317d3146463b converge over archives an earlier run already wrote ([@tj-smith47](https://github.com/tj-smith47))
* d579a0d1922c give each cpu variant of a target its own archive ([@tj-smith47](https://github.com/tj-smith47))
* 174c01fc3d7f ignore files entries and report an empty meta entry under binary format ([@tj-smith47](https://github.com/tj-smith47))
* 9040f54d7de0 name every binary from the artifact metadata or its file name ([@tj-smith47](https://github.com/tj-smith47))
* 1a01ae475bf6 name every binary-format output by the archive template and ignore extra files, matching GoReleaser ([@tj-smith47](https://github.com/tj-smith47))
* 02cda2128eae name the binary template var in the multi-binary collision remedy ([@tj-smith47](https://github.com/tj-smith47))
* f72a7be7a67a read every binary name through one metadata fallback ([@tj-smith47](https://github.com/tj-smith47))
* 42f4d754c775 skip templated files for a binary-only target ([@tj-smith47](https://github.com/tj-smith47))
* 29cc18b7bdc8 surface write-back errors when closing a release archive ([@tj-smith47](https://github.com/tj-smith47))
* 9c185e4a09d9 append the path separator only when the target lacks one ([@tj-smith47](https://github.com/tj-smith47))
* 37d819023b82 escape appended artifact names and keep query strings intact ([@tj-smith47](https://github.com/tj-smith47))
* 697ed484c5c7 reject two archives that map to the same PKGBUILD architecture ([@tj-smith47](https://github.com/tj-smith47))
* a1fd7455eccc single-quote every PKGBUILD field so quotes and spaces survive ([@tj-smith47](https://github.com/tj-smith47))
* a6acddaea343 default the binary name to the crate name when no build declares one ([@tj-smith47](https://github.com/tj-smith47))
* ff0400f06d78 derive the archive binary from every selector the archive stage applies ([@tj-smith47](https://github.com/tj-smith47))
* 34db2d4bf692 derive the package binary from the archive's own builds ([@tj-smith47](https://github.com/tj-smith47))
* 986eec7b9dba seed the target variables before rendering a per-target binary name ([@tj-smith47](https://github.com/tj-smith47))
* d48a188a47cb skip a meta archive when choosing the binstallable one ([@tj-smith47](https://github.com/tj-smith47))
* f4bf6cc0a348 reject AWS KMS payloads above the 4096-byte encrypt limit ([@tj-smith47](https://github.com/tj-smith47))
* 14ed4d4730ca declare rust-version 1.94 and prove the locked graph builds on it ([@tj-smith47](https://github.com/tj-smith47))
* 2c2d5a7b3fc3 derive the produced binary name from the shared helper ([@tj-smith47](https://github.com/tj-smith47))
* 70457a554eb1 prepare rustup targets in the build directory with the build env ([@tj-smith47](https://github.com/tj-smith47))
* 63855209bb62 print every manifest path relative to the repo in version_sync errors ([@tj-smith47](https://github.com/tj-smith47))
* b156270b148d print version_sync manifest paths relative to the repo ([@tj-smith47](https://github.com/tj-smith47))
* 5a4ab0d85af5 route the cross-toolchain hint's skip decision through the shared build planner ([@tj-smith47](https://github.com/tj-smith47))
* b9f9d217b4f6 heal an exclusive '>' dependency lower bound to '>=' so the cut version resolves ([@tj-smith47](https://github.com/tj-smith47))
* eb92a924ad36 stage every manifest the bump edited before committing ([@tj-smith47](https://github.com/tj-smith47))
* 3560cbeaca97 span the whole workspace in a lockstep release body ([@tj-smith47](https://github.com/tj-smith47))
* d75ae4d0e17c skip a crate whose windows archives are ambiguous instead of packaging whichever came first ([@tj-smith47](https://github.com/tj-smith47))
* bceba211955c read an empty tag_template as unset, like the accessor already does ([@tj-smith47](https://github.com/tj-smith47))
* 19cbe4758f3b say an installer stage defines no binary variable when two entries collide ([@tj-smith47](https://github.com/tj-smith47))
* 7d6a7b629fc8 stop doubling an explicit tag_prefix into PrefixedTag ([@tj-smith47](https://github.com/tj-smith47))
* bea413a6eeaa advise a distinct entry name when two config entries collide on one target ([@tj-smith47](https://github.com/tj-smith47))
* aaf94bbb4232 apply the skip predicate to the derived target list ([@tj-smith47](https://github.com/tj-smith47))
* 714e6d56f81a apply the skip predicate when naming a crate's primary binary ([@tj-smith47](https://github.com/tj-smith47))
* 6a6ad6d8b31c default the test context dist to a private tempdir ([@tj-smith47](https://github.com/tj-smith47))
* 60bd9aa57717 fail the executable stub probe on any exit but the guard's ([@tj-smith47](https://github.com/tj-smith47))
* da3e4b56425d free each test context's private dist directory when the context drops ([@tj-smith47](https://github.com/tj-smith47))
* db5aa33a90a5 match a secret on a word boundary instead of a minimum length ([@tj-smith47](https://github.com/tj-smith47))
* 40d60beb2d33 name the crate or target variable that actually separates colliding archive names ([@tj-smith47](https://github.com/tj-smith47))
* c70dca44a0af read a binary's name through one artifact accessor everywhere ([@tj-smith47](https://github.com/tj-smith47))
* cd32256ecbb7 redact a secret whose value carries carriage returns ([@tj-smith47](https://github.com/tj-smith47))
* 1f4d17f72ebb redact secrets that straddle a child process write boundary ([@tj-smith47](https://github.com/tj-smith47))
* a8387822e5c3 render a binary-fallback build id before matching an archive id ([@tj-smith47](https://github.com/tj-smith47))
* 45ee3a69022c say when the binary variable would also separate two colliding entries ([@tj-smith47](https://github.com/tj-smith47))
* 84542eccc092 share one ellipsis truncation between the release body and the drift summary ([@tj-smith47](https://github.com/tj-smith47))
* 6354790c997a treat only a test-only cfg as a gated test module ([@tj-smith47](https://github.com/tj-smith47))
* e92547511b0f write the tool version as anodizer_version in determinism.json ([@tj-smith47](https://github.com/tj-smith47))
* 47c86238a3c5 skip a manifest whose image template list is empty ([@tj-smith47](https://github.com/tj-smith47))
* 717a96cd6cbf skip a manifest whose image templates all render empty ([@tj-smith47](https://github.com/tj-smith47))
* 6a3dce9052fa quote both fields of the generated install commands ([@tj-smith47](https://github.com/tj-smith47))
* 18c7ac7caa90 keep tag listings one per line when git is configured to columnize output ([@tj-smith47](https://github.com/tj-smith47))
* 69525a7fa36b match the latest tag the way the tag_template spells it ([@tj-smith47](https://github.com/tj-smith47))
* c87323597c51 redact url credentials in the message a refused repository reports ([@tj-smith47](https://github.com/tj-smith47))
* f3de81b49d97 report git's own error when a repository check fails ([@tj-smith47](https://github.com/tj-smith47))
* 045a14b98d55 reject a gitea download URL without a scheme or host ([@tj-smith47](https://github.com/tj-smith47))
* 43172e362519 resolve the gitea API base in one core module ([@tj-smith47](https://github.com/tj-smith47))
* dd17164cd2ad mark git below the 2.13 floor instead of reporting it available ([@tj-smith47](https://github.com/tj-smith47))
* adf66c5dc626 escape interpolation, newlines and tabs in Ruby strings ([@tj-smith47](https://github.com/tj-smith47))
* d556dfbe6af6 normalise the cask token and file name ([@tj-smith47](https://github.com/tj-smith47))
* 3f90b4d529c7 stop writing the deprecated cask url.verified stanza ([@tj-smith47](https://github.com/tj-smith47))
* 9479a473c730 redact every hook output line against the hook's own environment ([@tj-smith47](https://github.com/tj-smith47))
* 627527604e90 redact secrets in the logged hook command line ([@tj-smith47](https://github.com/tj-smith47))
* b44baf1724dd load the config once per version-files enrolment ([@tj-smith47](https://github.com/tj-smith47))
* 86e3f0bf61d4 match existing gitignore entries by whole line ([@tj-smith47](https://github.com/tj-smith47))
* 774d7bc14d0e stop re-enrolling version files an includes file already declares ([@tj-smith47](https://github.com/tj-smith47))
* 2b841f7c2719 extract every archive format the release can ship ([@tj-smith47](https://github.com/tj-smith47))
* 09491c2b8ade install a bare binary whose asset name is the binary name ([@tj-smith47](https://github.com/tj-smith47))
* 81e25366ebe2 name the real script file and the matched case subject ([@tj-smith47](https://github.com/tj-smith47))
* 317913d3b525 warn about PATH for the directory actually installed to ([@tj-smith47](https://github.com/tj-smith47))
* 106e3a7f3725 accept a case line with a trailing comment or a hoisted subject ([@tj-smith47](https://github.com/tj-smith47))
* efad538b11c7 accept a hoisted subject declared with export, local or readonly ([@tj-smith47](https://github.com/tj-smith47))
* 78acc59c85fe drop the asset arm for a libc class the install script cannot probe ([@tj-smith47](https://github.com/tj-smith47))
* d25015224917 key install-script arms with the same libc classifier as asset names ([@tj-smith47](https://github.com/tj-smith47))
* 23a1d5173904 require a word boundary when a hoisted subject is expanded ([@tj-smith47](https://github.com/tj-smith47))
* 7f659031e175 require template installers to consume the libc case subject ([@tj-smith47](https://github.com/tj-smith47))
* 91b525346bd1 require the libc case subject in the template's case statement ([@tj-smith47](https://github.com/tj-smith47))
* c9975ccfbb6e split installer arms only when the libc builds ship different assets ([@tj-smith47](https://github.com/tj-smith47))
* 90249fd10894 default arm_variant to the documented 6 ([@tj-smith47](https://github.com/tj-smith47))
* 7e7c2c254958 drop the unsupported mcpb registry type ([@tj-smith47](https://github.com/tj-smith47))
* 7b373cbcdbcd drop the v1 microarchitecture suffix from deb architectures ([@tj-smith47](https://github.com/tj-smith47))
* d285fd11da07 map every 32-bit ARM variant to Termux's arm architecture ([@tj-smith47](https://github.com/tj-smith47))
* 6bdef9fcf5c4 resolve Debian arch names and the baseline amd64 level from core ([@tj-smith47](https://github.com/tj-smith47))
* 8f60547f2f81 compose the monorepo prefix with the crate's tag family ([@tj-smith47](https://github.com/tj-smith47))
* d4723f14ff3e name the excluded sibling families in the retention scope line ([@tj-smith47](https://github.com/tj-smith47))
* 5f8f5da04f93 never skip a repo's first nightly ([@tj-smith47](https://github.com/tj-smith47))
* 1e252717afb5 render Tag as the tag the nightly release is created on ([@tj-smith47](https://github.com/tj-smith47))
* c7ca12901ed7 scope the retention sweep to the publishing track's tag family ([@tj-smith47](https://github.com/tj-smith47))
* 833bbc04bc0d stop leaking a phantom nightly tag into every template ([@tj-smith47](https://github.com/tj-smith47))
* db4e004752bc take the previous tag from the base's own tag family ([@tj-smith47](https://github.com/tj-smith47))
* 67eb95d9a655 take the synthesized version's base across every tag family ([@tj-smith47](https://github.com/tj-smith47))
* b07b4b702821 wire nightly.tag_name and nightly.name_template ([@tj-smith47](https://github.com/tj-smith47))
* cd6a24478594 run the preInstall and postInstall hooks in the generated installPhase ([@tj-smith47](https://github.com/tj-smith47))
* 70aded93350d reject a macos notarization timeout past apple's twenty-minute ceiling ([@tj-smith47](https://github.com/tj-smith47))
* d25c98642f47 print a manifest under a symlinked repo root relative to the root ([@tj-smith47](https://github.com/tj-smith47))
* 6e7489b31b6b print repo-relative paths with forward slashes on Windows too ([@tj-smith47](https://github.com/tj-smith47))
* 6257c9edec30 fail when an explicit --version or --from-run matches no revision ([@tj-smith47](https://github.com/tj-smith47))
* f4b1e7c0d140 reject a malformed snap channel before touching the Snap Store ([@tj-smith47](https://github.com/tj-smith47))
* b4a35a4b6bef check preserved raw binaries exist in a publish-only run ([@tj-smith47](https://github.com/tj-smith47))
* fa2532c4131e drop the rollback evidence a dry run cannot have created ([@tj-smith47](https://github.com/tj-smith47))
* a2cc6cc22015 keep a publisher's landed outcome when only some of its entries skipped ([@tj-smith47](https://github.com/tj-smith47))
* 792cda5b7083 keep a versionless schema file out of the schemastore rekey path ([@tj-smith47](https://github.com/tj-smith47))
* fe7dfb0bfe20 keep source rpms when an ids filter narrows an upload target ([@tj-smith47](https://github.com/tj-smith47))
* b617cdde927b list a repeated entry-skip reason once in the publisher's skip line ([@tj-smith47](https://github.com/tj-smith47))
* 08d818331d68 name the anodizer binary correctly in the prior-report error ([@tj-smith47](https://github.com/tj-smith47))
* bb2f4e335a39 name the run summary's tool version anodizer_version ([@tj-smith47](https://github.com/tj-smith47))
* 0ac4e49ef9ba pair an entry-skip label to its evaluator by the rule production uses ([@tj-smith47](https://github.com/tj-smith47))
* cc2fe9d1c1e2 prefer the not-applicable reason over entries-skipped when nothing applied ([@tj-smith47](https://github.com/tj-smith47))
* 70612da2710e record a winget pull request only when the crate submits one ([@tj-smith47](https://github.com/tj-smith47))
* 7932c03ceeeb record an AUR rollback target only for a crate that pushed ([@tj-smith47](https://github.com/tj-smith47))
* e0fbca18e64f record an AUR rollback target per entry, inside the scope that pushed ([@tj-smith47](https://github.com/tj-smith47))
* a67249f6fd29 report a finished publish with no landed entry as such ([@tj-smith47](https://github.com/tj-smith47))
* 19f4f3bde245 report every entry skip reason and spell the run summary once ([@tj-smith47](https://github.com/tj-smith47))
* fa7c0b2a8759 resolve the crates.io sparse index through an injectable base ([@tj-smith47](https://github.com/tj-smith47))
* 80ea59fc13ba scope an entry skip to its own publisher's sub-labels ([@tj-smith47](https://github.com/tj-smith47))
* 2c51a7a8d83a send every release artifact kind to an artifactory or uploads target ([@tj-smith47](https://github.com/tj-smith47))
* f6611a8fc709 skip a disqualified crate in the pre-publish guard instead of aborting the release ([@tj-smith47](https://github.com/tj-smith47))
* 8dbf7519c43a skip a half-configured upload entry and report an all-skipped publisher as skipped ([@tj-smith47](https://github.com/tj-smith47))
* 78aaa8bd4fc9 skip a krew or scoop crate with no repository instead of failing the publisher ([@tj-smith47](https://github.com/tj-smith47))
* c9758d379f27 skip a misconfigured nix, brew, cask, winget or upload entry instead of stopping its siblings ([@tj-smith47](https://github.com/tj-smith47))
* f5bddfb59c11 skip an ambiguous AUR architecture instead of aborting the run ([@tj-smith47](https://github.com/tj-smith47))
* 3f52b9f299f9 stop sending a dry run to verify remote state it never created, and pin the nested summary section ([@tj-smith47](https://github.com/tj-smith47))
* 167fb1d3bb1e suggest the amd64 variant remedy only when a variant archive was produced ([@tj-smith47](https://github.com/tj-smith47))
* 1fb0d056c7f2 warn about a missing publish block only when no entry was skipped, and report a publisher skipped whenever one of its entries was ([@tj-smith47](https://github.com/tj-smith47))
* 4c49f79fa2a5 fail a custom publisher whose cmd is empty while artifacts match ([@tj-smith47](https://github.com/tj-smith47))
* d0dcc674bb63 count the always-mask length in characters and name the npm binary format once ([@tj-smith47](https://github.com/tj-smith47))
* e2c5caecbb2a mask a secret of eight characters or more wherever it appears ([@tj-smith47](https://github.com/tj-smith47))
* ac241152f7f4 anchor each crate's Tag before its release body renders ([@tj-smith47](https://github.com/tj-smith47))
* c3a01cafe5d4 bail on an empty release.tag instead of silently falling back ([@tj-smith47](https://github.com/tj-smith47))
* 4279157d3c39 compare the release.tag override against the tag that was actually pushed ([@tj-smith47](https://github.com/tj-smith47))
* 2c4ed1303259 count a split shard by its own identity even when it produced no artifacts ([@tj-smith47](https://github.com/tj-smith47))
* 9c748d36a15a count only files as dist population so a refused split run can retry ([@tj-smith47](https://github.com/tj-smith47))
* 4d0113a50d5a derive no release URL when the release stage is skipped ([@tj-smith47](https://github.com/tj-smith47))
* b1d5c1ead9c4 drop a foreign previous tag when a crate's tag lookup fails ([@tj-smith47](https://github.com/tj-smith47))
* 8226f9f397fa drop stale run bookkeeping from dist before a fresh run ([@tj-smith47](https://github.com/tj-smith47))
* 52a243d07945 fail when git refuses to read the repository instead of reporting no release tags ([@tj-smith47](https://github.com/tj-smith47))
* 22fb51c18fa0 fan an arch-qualified merge shard over its own matrix entries ([@tj-smith47](https://github.com/tj-smith47))
* fc4ca5c460b6 honour a short github retry-after instead of raising it to a minute ([@tj-smith47](https://github.com/tj-smith47))
* 7d31e830aaba join every release-body part with the one separator constant ([@tj-smith47](https://github.com/tj-smith47))
* 51a3c3e2c7a4 keep a nested sibling track's tag out of this crate's release and changelog range ([@tj-smith47](https://github.com/tj-smith47))
* 2ce6ffcd480a keep a triple-named split shard as its own worker key ([@tj-smith47](https://github.com/tj-smith47))
* 309bf1a9ac81 keep the Full Changelog link and footer when a release body is truncated and trim CRLF header endings ([@tj-smith47](https://github.com/tj-smith47))
* a4be4461e6f1 keep the new body's trailer when appending to an oversized release ([@tj-smith47](https://github.com/tj-smith47))
* b15b86e1e02a key split workers and their contexts on the one matrix axis ([@tj-smith47](https://github.com/tj-smith47))
* 3a2f386dcdf6 leave nothing but the run's own config behind after a dry-run ([@tj-smith47](https://github.com/tj-smith47))
* 274a24372781 let a failed run retry without --clean when dist holds only its own bookkeeping ([@tj-smith47](https://github.com/tj-smith47))
* eca0258f4352 let a rolling nightly tag replace its own assets ([@tj-smith47](https://github.com/tj-smith47))
* ea2b941576dd read anodizer's own artifacts.json when merging without context.json ([@tj-smith47](https://github.com/tj-smith47))
* 1e3392b37cf3 read manifest versions, artifacts.json and dist layouts through one reader each ([@tj-smith47](https://github.com/tj-smith47))
* 2468d7b1b3cb recover from an unreadable repository only in the modes that tolerate it ([@tj-smith47](https://github.com/tj-smith47))
* e73bd567a86c refuse a tag already published as an immutable github release before uploading anything ([@tj-smith47](https://github.com/tj-smith47))
* d8141dfd7e54 reject a Gitea API URL without a scheme or host ([@tj-smith47](https://github.com/tj-smith47))
* 6162b19a9fcd resolve every crate's tag family through one accessor ([@tj-smith47](https://github.com/tj-smith47))
* 2b4abc46398d resolve the nightly tag once, from one tag-family accessor ([@tj-smith47](https://github.com/tj-smith47))
* 734976de3197 resolve the release.tag override before the declared tag, per crate family ([@tj-smith47](https://github.com/tj-smith47))
* 6d1916a2f4b1 route download URLs and the install script through the one tag resolver ([@tj-smith47](https://github.com/tj-smith47))
* 05e1ba532cde run the pre-publish guard before the release is created ([@tj-smith47](https://github.com/tj-smith47))
* 420eb2901c66 share one empty release.tag message across install script, binstall and release ([@tj-smith47](https://github.com/tj-smith47))
* 6c1ce3d5af84 skip run bookkeeping only where the run writes it ([@tj-smith47](https://github.com/tj-smith47))
* 05c81f54d069 stop doubling /api/v1 in self-hosted Gitea API URLs ([@tj-smith47](https://github.com/tj-smith47))
* 50adacf78f1e stop the github-release publisher re-running the release stage ([@tj-smith47](https://github.com/tj-smith47))
* 42afaba618ed warn again when release.tag diverges from the pushed tag ([@tj-smith47](https://github.com/tj-smith47))
* 8a178d76ea67 attribute backoff to the scope that incurred it instead of the last one entered ([@tj-smith47](https://github.com/tj-smith47))
* 2c30edd84a9e disambiguate default binary SBOM names by microarchitecture variant ([@tj-smith47](https://github.com/tj-smith47))
* 8d0bf573ce9a anchor JSONC keys structurally and complete the unknown-format derivation ([@tj-smith47](https://github.com/tj-smith47))
* bd1012b09f24 match the upstream catalog entry by name and declare a vendored schema's unknown formats ([@tj-smith47](https://github.com/tj-smith47))
* fd7354c386c1 classify a config that matches nothing from its rendered arguments everywhere ([@tj-smith47](https://github.com/tj-smith47))
* 1719ee084f61 classify a re-verification config from the shared empty-match render ([@tj-smith47](https://github.com/tj-smith47))
* d16b0858e851 decide keyless cosign from the rendered command and arguments ([@tj-smith47](https://github.com/tj-smith47))
* 10d3a5162c50 hand the signer the same output path the signature artifact registers ([@tj-smith47](https://github.com/tj-smith47))
* 442dd505e728 hold the host TUF lock for every distinct cache root a keyless run signs against ([@tj-smith47](https://github.com/tj-smith47))
* bc2732887505 key the host TUF lock on the canonical cache path ([@tj-smith47](https://github.com/tj-smith47))
* d222b78cd7d6 lock every per-image TUF root before signing docker images ([@tj-smith47](https://github.com/tj-smith47))
* 73616a79af98 name the canonical sentinel path in the TUF lock failure message ([@tj-smith47](https://github.com/tj-smith47))
* 211f23e5943c record the harness skip for a keyless config that matches nothing ([@tj-smith47](https://github.com/tj-smith47))
* 9f6134ad44bf report lock contention and keep the docker env overlay when one entry is per-image ([@tj-smith47](https://github.com/tj-smith47))
* f3b66c171018 serialize keyless cosign and hold the host TUF lock at every keyless spawn site ([@tj-smith47](https://github.com/tj-smith47))
* 8091880842d8 sign macOS universal binaries under the default artifacts filter ([@tj-smith47](https://github.com/tj-smith47))
* 65795defde92 warn once per artifact that is missing its signature ([@tj-smith47](https://github.com/tj-smith47))
* 675e8d189479 warn when a signer exits 0 without writing its output file ([@tj-smith47](https://github.com/tj-smith47))
* d0165dec3d39 emit top-level assumes, hooks and plugs without an apps block ([@tj-smith47](https://github.com/tj-smith47))
* e4dc93561927 add each extra file once, under one path rule for every archive format ([@tj-smith47](https://github.com/tj-smith47))
* b8a3f82d5c3e add each extra file to the source zip once under its relative path ([@tj-smith47](https://github.com/tj-smith47))
* acb3546073c8 keep executable permissions on rewritten source zip entries ([@tj-smith47](https://github.com/tj-smith47))
* f2023212e5c8 report extras already in the archive once, and name them relative to the root ([@tj-smith47](https://github.com/tj-smith47))
* ebb9fba0956d bump the manifest on the repo-level tag path ([@tj-smith47](https://github.com/tj-smith47))
* 5d9dc8188f30 cut a one-crate repo's tag in the crate's own tag family ([@tj-smith47](https://github.com/tj-smith47))
* d5c7ba4672f5 derive one tag family for a workspace that releases under a single tag ([@tj-smith47](https://github.com/tj-smith47))
* fccfe470b008 heal every internal path+version dep floor in the bump commit ([@tj-smith47](https://github.com/tj-smith47))
* ddd047bc9639 match every version_files anchor against the original file before rewriting ([@tj-smith47](https://github.com/tj-smith47))
* 29a38de93be2 name the run-summary artifact fetch when rollback refuses on a published release ([@tj-smith47](https://github.com/tj-smith47))
* 8e89e933312d preview exactly the dep-floor heals the bump commit will make ([@tj-smith47](https://github.com/tj-smith47))
* d29f6617967a print version_files paths relative to the repo in every message ([@tj-smith47](https://github.com/tj-smith47))
* a82f3c99ee8f render the repo-level manifest refusal on one line and name a missing or unparseable manifest ([@tj-smith47](https://github.com/tj-smith47))
* a6b920aefd12 resolve a top-level version_files version the way check version-files does ([@tj-smith47](https://github.com/tj-smith47))
* 84b1ae3ce488 resolve every version_files owner through one seam shared with check ([@tj-smith47](https://github.com/tj-smith47))
* a890eb9a137c rewrite identical version_files pairs from one owner instead of refusing them ([@tj-smith47](https://github.com/tj-smith47))
* f11ee62fe79d rewrite top-level version_files in a single-crate config ([@tj-smith47](https://github.com/tj-smith47))
* 92f17f92ba54 run the version_files guard in every config mode ([@tj-smith47](https://github.com/tj-smith47))
* 1c0bf67f267b state only what the repo-level bump verified when a manifest declares no version ([@tj-smith47](https://github.com/tj-smith47))
* 03d03b044cf6 tag a one-crate repo in the crate's own tag family ([@tj-smith47](https://github.com/tj-smith47))
* a0df42a14d1b treat an identical version_files pair as one rewrite whatever its owners ([@tj-smith47](https://github.com/tj-smith47))
* 023e869c60fd keep wasm32 out of the Debian architecture map ([@tj-smith47](https://github.com/tj-smith47))
* a5d3f483871f map powerpc and wasm32 to their canonical Go architectures ([@tj-smith47](https://github.com/tj-smith47))
* 3e3994e9f952 root a test context at a path that does not exist instead of the working directory ([@tj-smith47](https://github.com/tj-smith47))
* f9e332bf4571 widen the shared source splitter and walk detector to the spellings this workspace uses ([@tj-smith47](https://github.com/tj-smith47))
* ad279d82ef2d exclude skipped crates from the pre-publish gate ([@tj-smith47](https://github.com/tj-smith47))
* 769297731c02 bound words inside the anchored region and fail a stale anchor on a no-op bump ([@tj-smith47](https://github.com/tj-smith47))
* 51bcacc943a4 derive the automatic package identifier at one seam ([@tj-smith47](https://github.com/tj-smith47))
* bee38837ecd5 fail a package identifier that does not render instead of polling with the raw template ([@tj-smith47](https://github.com/tj-smith47))
* 9c07cb48bd7e pin the package identifier to a single render seam ([@tj-smith47](https://github.com/tj-smith47))
* a535661d73cf record no landed entry for a dry-run submission ([@tj-smith47](https://github.com/tj-smith47))
* 0622592770fc render package_identifier once when the config is derived ([@tj-smith47](https://github.com/tj-smith47))
* 25e3852a15a5 render package_identifier templates before validating them ([@tj-smith47](https://github.com/tj-smith47))
* b877a068a4a1 render the repository owner once per crate ([@tj-smith47](https://github.com/tj-smith47))
* 94612865439c spell the tool name anodizer in every message, help string and doc ([@tj-smith47](https://github.com/tj-smith47))
* 882ef704adbd give the binary format one name and one .exe rule ([@tj-smith47](https://github.com/tj-smith47))
* df04d85efc45 let each output branch name its own artifact when claiming a path ([@tj-smith47](https://github.com/tj-smith47))
* 2299ed908d39 pass run-constant plan inputs as one struct ([@tj-smith47](https://github.com/tj-smith47))
* 9373784a9ded share one target-variant key and one container close helper ([@tj-smith47](https://github.com/tj-smith47))
* c6dad60036bc derive the synthesized default build's binary from the shared fallback helper ([@tj-smith47](https://github.com/tj-smith47))
* 0884863ea942 give the build skip gate a strict form and take paths as Path ([@tj-smith47](https://github.com/tj-smith47))
* cbcd5fd8c078 answer the --crate selection question from one accessor ([@tj-smith47](https://github.com/tj-smith47))
* 203d76ffa4c8 derive the fallback binary name from one helper ([@tj-smith47](https://github.com/tj-smith47))
* 70504cc7e43a drop the unused stdin-drain rung from the fake-tool builder ([@tj-smith47](https://github.com/tj-smith47))
* e7da527509bd hold the private test dist directory without an Arc ([@tj-smith47](https://github.com/tj-smith47))
* aca334b24a78 name the env-secret predicate once and keep the fallback tag family crate-private ([@tj-smith47](https://github.com/tj-smith47))
* db460ed17948 pass archive-name claims as one struct and advise only variables the stage exposes ([@tj-smith47](https://github.com/tj-smith47))
* daaeb4a0cc1b resolve build skips, tag families and name defaults from one helper each ([@tj-smith47](https://github.com/tj-smith47))
* e0a6a403ab2f route the last four crate-selection reads through the shared accessor ([@tj-smith47](https://github.com/tj-smith47))
* a34770189325 share one tracing capture helper across the crate's tests ([@tj-smith47](https://github.com/tj-smith47))
* 4b3ed134b3b3 share the test-source predicate and the gated-module premise check across crates ([@tj-smith47](https://github.com/tj-smith47))
* 2708b8cd4677 walk rust sources through the shared scanner ([@tj-smith47](https://github.com/tj-smith47))
* 4caf622a28a9 fold the arm table by platform key through one helper ([@tj-smith47](https://github.com/tj-smith47))
* e0a3fddf14dc keep the renderer internals out of the shipped surface and stop copying the ANSI stripper ([@tj-smith47](https://github.com/tj-smith47))
* e51238df1599 derive the retention scope's exclusions from the one family matcher ([@tj-smith47](https://github.com/tj-smith47))
* 0f24a5e4ac34 compute the xar table-of-contents checksum with the sha1 crate ([@tj-smith47](https://github.com/tj-smith47))
* 65a4ec49d889 compose the shared target enumeration instead of re-deriving it ([@tj-smith47](https://github.com/tj-smith47))
* d42e26af2363 replace four pieces of figurative jargon with plain words ([@tj-smith47](https://github.com/tj-smith47))
* 810c01a1c8ed compute the skip outcome as a value before recording it ([@tj-smith47](https://github.com/tj-smith47))
* e5557dec399a decide the not-applicable precedence by value instead of statement order ([@tj-smith47](https://github.com/tj-smith47))
* 3b0bbde9df03 read a call's string literals through one lexer ([@tj-smith47](https://github.com/tj-smith47))
* 3b4d88c36486 read the krew arm variant from one helper ([@tj-smith47](https://github.com/tj-smith47))
* 1c8e4b83cff2 collapse the duplicated merge tail ([@tj-smith47](https://github.com/tj-smith47))
* 40ebab9a6b38 name the rendered docker sign fields instead of a tuple ([@tj-smith47](https://github.com/tj-smith47))
* b905204d0a52 read the one-crate tag family from the single accessor ([@tj-smith47](https://github.com/tj-smith47))
* 9439d012a479 read the repo tag prefix through the one config accessor ([@tj-smith47](https://github.com/tj-smith47))
* 7a92bed1e304 refresh Cargo.lock through one helper on every bump path ([@tj-smith47](https://github.com/tj-smith47))
* 1967d6ed9751 rename ResolvedConfig::from_tag_config to from_config ([@tj-smith47](https://github.com/tj-smith47))

---
### Performance

* 3298a097ad87 fetch the dialect allowlist only when a vendor schema needs it ([@tj-smith47](https://github.com/tj-smith47))

## [0.25.2] - 2026-07-29

### Bug Fixes

* 33c33e69b1e5 reject an <Environment> wxs on a wixl too old to parse it ([@tj-smith47](https://github.com/tj-smith47))
* 09c47e98e003 drive cpio directly instead of a pipefail shell pipeline ([@tj-smith47](https://github.com/tj-smith47))

## [0.25.1] - 2026-07-27

### Bug Fixes

* fffc776b75d0 retry a dropped connection on every publisher git push ([@tj-smith47](https://github.com/tj-smith47))

## [0.25.0] - 2026-07-27

### Features

* ee2635f3a376 keep nightly off 'latest' and scope its publishers ([@tj-smith47](https://github.com/tj-smith47))

## [0.24.0] - 2026-07-26

### Features

* 3b523c2f4d3b add a root always: hook list that fires last on every path ([@tj-smith47](https://github.com/tj-smith47))
* 5420467d5b20 give anodizer build the after: and always: teardown lanes ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 6957f524bf4b stop a dry-run failing on the previous run's artifacts ([@tj-smith47](https://github.com/tj-smith47))
* fdffdce0ed28 scope the previous-tag search to the cut tag's own family ([@tj-smith47](https://github.com/tj-smith47))
* 0737b271d243 one --skip token per root lane, honored at every site ([@tj-smith47](https://github.com/tj-smith47))
* e80df3505535 fire the root after: block once per run, not once per crate ([@tj-smith47](https://github.com/tj-smith47))
* 38657065408f bound every MCP registry call by the retry wall-clock budget ([@tj-smith47](https://github.com/tj-smith47))
* 1ac87ef164ab bound the cargo and pypi OIDC token hops by the retry budget ([@tj-smith47](https://github.com/tj-smith47))
* 44c6810416bc skip the built-package cross-check in a dry-run ([@tj-smith47](https://github.com/tj-smith47))
* ee427df6bfe1 attribute stage backoff to the stage that incurred it ([@tj-smith47](https://github.com/tj-smith47))
* 183ee2f4f4f1 make the wall-clock retry budget one value per publisher invocation ([@tj-smith47](https://github.com/tj-smith47))
* 72a27e879c46 give every registered builtin a Go positional form ([@tj-smith47](https://github.com/tj-smith47))
* 30e294dc4411 honor raw blocks, rewrite every pipeline segment, and cap nesting depth ([@tj-smith47](https://github.com/tj-smith47))
* 25d5e67cb58f rewrite Go calls in range and assignment blocks too ([@tj-smith47](https://github.com/tj-smith47))
* 1174b24ed40a rewrite Go sub-expression arguments recursively ([@tj-smith47](https://github.com/tj-smith47))
* 815bc8e91a19 split artifact.rs along the kind/registry/filter/report seam ([@tj-smith47](https://github.com/tj-smith47))
* 2514724e0f3f split git/tags.rs along the family / discover / previous / position / mutate seam ([@tj-smith47](https://github.com/tj-smith47))
* 758adcb2e06d split log.rs into a log/ module dir ([@tj-smith47](https://github.com/tj-smith47))
* 9aa94250a6c7 split run.rs along the exec / process-tree seam ([@tj-smith47](https://github.com/tj-smith47))
* dfcdf0c24fa0 split publisher.rs along the resolve/publish/publisher seam ([@tj-smith47](https://github.com/tj-smith47))
* 528077d7f033 fold the github push-probe arguments into a params struct ([@tj-smith47](https://github.com/tj-smith47))
* 1a3353080f8c converge PR-mode reconcile on one target builder per publisher ([@tj-smith47](https://github.com/tj-smith47))

## [0.23.0] - 2026-07-25

### Features

* e17cadaef502 delete --allow-rerun and the end-of-pipeline rerun guard ([@tj-smith47](https://github.com/tj-smith47))
* 1e14cd6cbd79 add convergent reconcile primitive to publisher dispatch ([@tj-smith47](https://github.com/tj-smith47))
* 68f165e63a1a reconcile npm and pypi against the registry before publishing ([@tj-smith47](https://github.com/tj-smith47))
* ade869d262b6 reconcile open PRs for every PR-mode manager publisher ([@tj-smith47](https://github.com/tj-smith47))
* db32ec03ce0f converge partial releases on re-run instead of rolling them back ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 1b4223345f40 never ship a bare or phantom-heading empty changelog on the git/SCM path ([@tj-smith47](https://github.com/tj-smith47))
* d8eb387e33c8 scope the reconcile probe and stop gating on an already-released version ([@tj-smith47](https://github.com/tj-smith47))
* 01bbcabae4a9 exempt anodizer's own binstall writes from the clean-tree guard ([@tj-smith47](https://github.com/tj-smith47))
* af5b4fcef540 record reconcile divergence in the report instead of bubbling Err ([@tj-smith47](https://github.com/tj-smith47))
* dde0a6abf26a build flip_one octocrab client inside its tokio runtime ([@tj-smith47](https://github.com/tj-smith47))
* 1a145b0298b6 never let stranded nightly tags drive stable selection or tag resolution ([@tj-smith47](https://github.com/tj-smith47))
* f2cab118e93a resolve each crate's own version when uploading snaps ([@tj-smith47](https://github.com/tj-smith47))
* 58bd0731c170 split run.rs god-file into run/ submodules ([@tj-smith47](https://github.com/tj-smith47))
* 8bdd6b2701df split run.rs god-file into run/ submodules ([@tj-smith47](https://github.com/tj-smith47))
* ff2387f321cf restore verbatim ROLLBACK_PARALLELISM comment from Wave-4 split ([@tj-smith47](https://github.com/tj-smith47))
* 20dbfaa7eef7 split artifactory.rs into artifactory/ dir ([@tj-smith47](https://github.com/tj-smith47))
* fd223598efca split scoop.rs into scoop/ dir ([@tj-smith47](https://github.com/tj-smith47))
* ef949e6e2c21 split process.rs god-file into process/ submodules ([@tj-smith47](https://github.com/tj-smith47))
* 2cac8861d91a extract lib.rs god-file into submodules ([@tj-smith47](https://github.com/tj-smith47))

## [0.22.2] - 2026-07-20

### Bug Fixes

* bdcdb7606186 abort OIDC publish up front when a crate was never published ([@tj-smith47](https://github.com/tj-smith47))
* 498da907e5ad stop deriving publish-order edges from unversioned dev-deps ([@tj-smith47](https://github.com/tj-smith47))
* b6203253e464 guard promote --from-run against path traversal ([@tj-smith47](https://github.com/tj-smith47))
* 7fa23abfad2f stop silently dropping a libc when gnu and musl target the same arch ([@tj-smith47](https://github.com/tj-smith47))
* 5eaf608ff713 honor if_condition in uploads rollback target collection ([@tj-smith47](https://github.com/tj-smith47))
* cd36c48cb8b5 record snap revision per arch and promote every architecture ([@tj-smith47](https://github.com/tj-smith47))
* 48b9384f5d62 stop misclassifying connectivity/auth faults as an absent snap ([@tj-smith47](https://github.com/tj-smith47))
* 1ac2ff37d536 split four god-files into cohesive siblings ([@tj-smith47](https://github.com/tj-smith47))
* a9e359d22706 split four Wave-1 god-files into cohesive submodules ([@tj-smith47](https://github.com/tj-smith47))
* 8e70a325c127 split four Wave-2 god-files into cohesive submodules ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* dfb25aa21e9c derive publish order from Cargo.toml when depends_on unresolved ([@tj-smith47](https://github.com/tj-smith47))
* bf8304a306d5 include versioned dev-dependencies in derived publish order ([@tj-smith47](https://github.com/tj-smith47))
* b56ea5ab4a60 demote backfill dist-tag to release-<version>, not bare semver ([@tj-smith47](https://github.com/tj-smith47))
* 8b7fae949b62 map darwin-universal target to universal2 wheel tag ([@tj-smith47](https://github.com/tj-smith47))
* 59b39f59407f exempt surface-dependent checksums from cross-leg byte-compare #none ([@tj-smith47](https://github.com/tj-smith47))
* aba7c6a493cb fix orphaned rustdoc and stale comments from Wave-1 split ([@tj-smith47](https://github.com/tj-smith47))
* d7a406f9e565 reword rule-9 caller-ref and pronoun comments from Wave-3 split ([@tj-smith47](https://github.com/tj-smith47))

## [0.22.1] - 2026-07-18

### Bug Fixes

* 2cdcea12c62b re-sign combined checksums after publish-time refresh ([@tj-smith47](https://github.com/tj-smith47))
* a4ca534e5772 split five oversized command modules into submodules ([@tj-smith47](https://github.com/tj-smith47))
* 2fc8c0e7d635 split seven oversized publisher modules into submodules ([@tj-smith47](https://github.com/tj-smith47))
* 10b3a133fd1c split flatpak/pkg/sbom/snapcraft stage god files into submodules ([@tj-smith47](https://github.com/tj-smith47))

## [0.20.0] - 2026-07-14

### Features

* 2e8e602ba1b5 add split_format (bare|coreutils) for shasum -c sidecars ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* c3d93f5b31a0 resume a completable partial crates.io publish instead of aborting ([@tj-smith47](https://github.com/tj-smith47))

## [0.19.0] - 2026-07-13

### Features

* bb3eb89d0e08 batteries-included curl|sh installer on the installer.rs engine ([@tj-smith47](https://github.com/tj-smith47))
* 664fae3e2dc2 cross-publisher track promotion without rebuild ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* bbbae08a82f0 tag fully-static linux-gnu binaries as manylinux, not hard-error ([@tj-smith47](https://github.com/tj-smith47))
* 363702951032 run before-hooks in --split and --merge modes ([@tj-smith47](https://github.com/tj-smith47))

## [0.18.0] - 2026-07-13

### Features

* 83cac3f9df1c finish the formula-bump publisher — tests, docs, indexes ([@tj-smith47](https://github.com/tj-smith47))
* 2564553652da skip_metapackage + platform_name_template for platform-only distribution ([@tj-smith47](https://github.com/tj-smith47))
* ce5f3a6c87e6 crates.io OIDC Trusted Publishing; split cargo.rs into cargo/ ([@tj-smith47](https://github.com/tj-smith47))
* 7e09a5fd8ab9 npm multi-command bins, PyPI wheels + Trusted Publishing (OIDC), homebrew-core ([@tj-smith47](https://github.com/tj-smith47))
* 539b0ec40309 native binary wheels + maturin sdist publisher ([@tj-smith47](https://github.com/tj-smith47))
* 9b34a450dcb2 sign version tags with the git-config signing key ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* a4691ee79b19 drain review findings — git-form bump correctness, lazy repo_info, rollback token fidelity, npm preflight+token ([@tj-smith47](https://github.com/tj-smith47))
* 131f29bc02a7 review fixes — single-source naming vars, uniform validation, mode gate, preflight skip_metapackage ([@tj-smith47](https://github.com/tj-smith47))
* ba3d5eaa50f8 homebrew git-form detection survives an inline-commented url line ([@tj-smith47](https://github.com/tj-smith47))
* d3c57c68a8ef make partial-rollback test hermetic #none ([@tj-smith47](https://github.com/tj-smith47))
* 99e25c964db7 regroup npm + pypi to Submitter so the rollback guard sees their burn #C14 ([@tj-smith47](https://github.com/tj-smith47))
* 640f60c59a87 drain review findings — tags, metadata, uploads, shared helpers ([@tj-smith47](https://github.com/tj-smith47))
* f5b54152c8cf close three burn-guard false-negatives that let a poisoning re-cut through ([@tj-smith47](https://github.com/tj-smith47))
* 689eb3306434 probe npm + pypi for burned versions in the unsummarized guard path ([@tj-smith47](https://github.com/tj-smith47))

## [0.17.0] - 2026-07-12

### Features

* 5a21a5ae1464 fail fast on provably-doomed plain-cargo cross gnu builds ([@tj-smith47](https://github.com/tj-smith47))
* 9d14a5a2e8c5 msix format and Termux-native arch naming (GoReleaser cd5f16b, 99a7173) ([@tj-smith47](https://github.com/tj-smith47))
* c9c9cc872257 mirror GoReleaser upstream — docker build retry breadth, release-repo token, versioned PR branches, winget default_locale ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 247f062ef2d3 dry-run must not abort on a doomed cross-gnu plan ([@tj-smith47](https://github.com/tj-smith47))
* 42c014f216b6 live-acceptance fixes for termux.deb and msix ([@tj-smith47](https://github.com/tj-smith47))
* e817829d897b gate verify-release install-smoke to OS-package publishers ([@tj-smith47](https://github.com/tj-smith47))
* 97a05071b795 gate verify/preflight axes to the selected publish surface ([@tj-smith47](https://github.com/tj-smith47))
* e5dbbefbc069 one gpg predicate, a faked-system-time preflight, and honest skips ([@tj-smith47](https://github.com/tj-smith47))
* fda1fdfb3a22 close the surface-gating and vacuous-verdict holes #minor ([@tj-smith47](https://github.com/tj-smith47))
* 25ee7153d259 stamp no verdict when the OS-package axes inspect zero packages ([@tj-smith47](https://github.com/tj-smith47))
* 4141cd8ae479 remove the vestigial gpg --faked-system-time preflight probe ([@tj-smith47](https://github.com/tj-smith47))

## [0.16.1] - 2026-07-10

### Bug Fixes

* 373b8af9f5c2 chocolatey nuspec drops the unsupported license element (CHCU0002) ([@tj-smith47](https://github.com/tj-smith47))
* 7400c1241b67 scoop manifests default into bucket/ (root manifests are invisible to scoop) ([@tj-smith47](https://github.com/tj-smith47))
* 3a576f321ade scope preflight state-probes to selected publishers ([@tj-smith47](https://github.com/tj-smith47))

## [0.16.0] - 2026-07-10

### Features

* dd75f87b05e7 classify deterministic failures with a machine-readable exit contract ([@tj-smith47](https://github.com/tj-smith47))
* 934cd7f64880 liveness heartbeat during slow subprocess waits ([@tj-smith47](https://github.com/tj-smith47))
* ea0cdaef2174 gate one-way-door publishers on burn probes and changelog provenance ([@tj-smith47](https://github.com/tj-smith47))
* 16b341e40f6b link anodizer in submission footers and generated-file headers ([@tj-smith47](https://github.com/tj-smith47))
* 380c392d8066 verify landed release assets and harden the tagless build path ([@tj-smith47](https://github.com/tj-smith47))
* c59a4b26df1e account run-wide retry backoff and surface it in the summary ([@tj-smith47](https://github.com/tj-smith47))
* f7279ea0fe38 add RetryStep step-retry engine with unified log lifecycle ([@tj-smith47](https://github.com/tj-smith47))
* 48e34f50791e add deadline-aware HTTP retry wrappers ([@tj-smith47](https://github.com/tj-smith47))
* f17957d2cd56 attribute retry backoff per publisher/stage in the summary ([@tj-smith47](https://github.com/tj-smith47))
* 333b361f22bc bound publisher retry ladders by the run's retry budget ([@tj-smith47](https://github.com/tj-smith47))
* e13ef744a394 resolve a raisable default retry budget and add an async deadline ([@tj-smith47](https://github.com/tj-smith47))
* 8ee354f2fb5e host-level TUF warm-up lock and warm-cache fast path ([@tj-smith47](https://github.com/tj-smith47))
* 79c859dadc5a surface Snap Store review holds and verify store landing ([@tj-smith47](https://github.com/tj-smith47))
* b292588a2a0d wire the liveness heartbeat into slow stage waits ([@tj-smith47](https://github.com/tj-smith47))
* daf93cbb6e42 emission-validate accountability on shards and dry-run URL derivation ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 77f4885d8f96 re-verify cargo publish discriminators on 1.97 and deflake the ticker test ([@tj-smith47](https://github.com/tj-smith47))
* c04a6eaa151c force 0755 on binaries staged into the dockers_v2 build context ([@tj-smith47](https://github.com/tj-smith47))
* 3e99866f4ee7 accept crates.io policy-denial 403s as proof of token validity ([@tj-smith47](https://github.com/tj-smith47))
* 2c48944c6857 harden tagging, rollback, retry visibility, and failure-path reporting ([@tj-smith47](https://github.com/tj-smith47))
* 96e42bde6d5b make bare `tag` fully local in every config mode ([@tj-smith47](https://github.com/tj-smith47))
* d1a235ad9bd8 attribute release cleanup on rollback and keep API tagging local when not pushing ([@tj-smith47](https://github.com/tj-smith47))
* f38944515e47 gate snapcraft review-hold stub tests to unix ([@tj-smith47](https://github.com/tj-smith47))
* ded0164d4a08 distinguish indeterminate landing probes and refuse identity-widening keyless verify ([@tj-smith47](https://github.com/tj-smith47))
* 402a9ea7d663 route all six retry forks through the shared step engine ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.5] - 2026-07-07

### Bug Fixes

* 22571b14e0d6 declare every run-path tool in publisher requirements ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.4] - 2026-07-07

### Bug Fixes

* 3afd0f214e79 serialize keyless cosign TUF init and retry transient failures with jittered backoff ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.3] - 2026-07-07

### Bug Fixes

* edaa7f88cb07 make same-version re-cuts survivable end to end ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.1] - 2026-07-06

### Bug Fixes

* 20f3db081207 allow http scheme in blob rollback delete client ([@tj-smith47](https://github.com/tj-smith47))
* 72daf41a9c1a anchor .cargo_vcs_info.json normalization to the crate root ([@tj-smith47](https://github.com/tj-smith47))
* 68f887b3f747 bound npm retry wall-time under the publish-npm job timeout ([@tj-smith47](https://github.com/tj-smith47))
* 4abf66e56f7d compare re-cut crates modulo .cargo_vcs_info.json so same-source re-cuts skip clean ([@tj-smith47](https://github.com/tj-smith47))
* 57eb14a1345a count only Normal path components for the crate-root vcs-info gate ([@tj-smith47](https://github.com/tj-smith47))
* 5eb94287af6b give git-revert rollback a commit identity ([@tj-smith47](https://github.com/tj-smith47))
* 38229cd0a3c9 skip schemastore schemas not owned by the current publish leg ([@tj-smith47](https://github.com/tj-smith47))
* 5e74097f11e9 surface npm retry-budget exhaustion and correct the metapackage install docs ([@tj-smith47](https://github.com/tj-smith47))
* 034b9b0f171e neutralize git config before clone in revert no-identity test ([@tj-smith47](https://github.com/tj-smith47))

## [0.15.0] - 2026-07-06

### Features

* 5f37d22b9b7c add per-publisher on_rollback failure hook ([@tj-smith47](https://github.com/tj-smith47))
* a54143cb7658 expose rollback trigger reason to on_rollback hooks ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 42790451f866 provision clang-cl+nasm defensively; correct guard comment; drop stray test assert ([@tj-smith47](https://github.com/tj-smith47))
* 2f8ed2c7823b keep --split --single-target composable (revert over-broad conflict) ([@tj-smith47](https://github.com/tj-smith47))
* a36a24b4db7d make required_failure_reason message extraction exhaustive ([@tj-smith47](https://github.com/tj-smith47))
* fe42673455fc add clang-cl C-toolchain pin primitive for windows-msvc ([@tj-smith47](https://github.com/tj-smith47))
* 75e77b27cc64 pin clang-cl in stage-build for windows-msvc release builds ([@tj-smith47](https://github.com/tj-smith47))
* 2d6e41c9bff1 pin clang-cl in the harness child env + hard-require it for windows-msvc ([@tj-smith47](https://github.com/tj-smith47))
* dcc962211412 per-shard self-skip, not wholesale skip ([@tj-smith47](https://github.com/tj-smith47))
* 38b618520256 OS-filter formula artifacts + gate emission-validate skip on partial shard ([@tj-smith47](https://github.com/tj-smith47))
* bbe4ef0623f1 wire the documented $ANODIZER_ARTIFACT env channel ([@tj-smith47](https://github.com/tj-smith47))
* 48f914cd3627 close AUR + homebrew failure-hiding on full builds lacking eligible archives ([@tj-smith47](https://github.com/tj-smith47))
* c50e732284ca exclude Apple-non-macOS archives from nix/krew/cask/npm ([@tj-smith47](https://github.com/tj-smith47))
* d9ca19d88500 gate index/manifest validator no-artifact skips on restricted builds ([@tj-smith47](https://github.com/tj-smith47))
* ee51d58ccf79 gate nix validator no-artifact skip on restricted builds ([@tj-smith47](https://github.com/tj-smith47))
* 150b5c8936e1 drop dead per-target CARGO_TARGET_<T>_RUSTFLAGS injection ([@tj-smith47](https://github.com/tj-smith47))
* 742a31d93983 route cask darwin-selection through is_macos ([@tj-smith47](https://github.com/tj-smith47))

## [0.14.0] - 2026-07-04

### Features

* 9660d545de42 migrate tera 1.20 -> 2.0 via JSON-boundary adapter ([@tj-smith47](https://github.com/tj-smith47))
* 315fa84bc67f verify TLS against the system trust store (reqwest 0.13, object_store 0.14) ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 383235b500c4 register .AppImage.zsync sidecar so it ships ([@tj-smith47](https://github.com/tj-smith47))
* b6f025d9b09f centralize conventional classifier, config discovery, tag-prefix fallback, release-commit subjects ([@tj-smith47](https://github.com/tj-smith47))
* 9f8723d8164e bump previews root-level crates; unify config discovery + token hints ([@tj-smith47](https://github.com/tj-smith47))
* 98cb49f06f6b finish the crate-universe conversion across the core + CLI tier ([@tj-smith47](https://github.com/tj-smith47))
* b6ae0325207e one shared aggregate-name rule for tag/changelog; harden --crate surfaces ([@tj-smith47](https://github.com/tj-smith47))
* 073e80c430b2 scope --workspace runs to the workspace; validate --crate selections; aggregate mixed-shape shared-prefix tags ([@tj-smith47](https://github.com/tj-smith47))
* 735f4654b981 validate --crate selections everywhere, scope workspace runs, group tag aggregation by prefix ([@tj-smith47](https://github.com/tj-smith47))
* eec56c00e405 one GitHub rate-limit detector; stop misclassifying 403s ([@tj-smith47](https://github.com/tj-smith47))
* f5197c73a782 walk the crate universe in per-crate hooks + release notes; centralize find_crate and archive shaping ([@tj-smith47](https://github.com/tj-smith47))
* 41437e5eeda8 build configured dockers_v2 dockerfile + extra_files, not repo-root Dockerfile ([@tj-smith47](https://github.com/tj-smith47))
* 49e29fd9440f mirror child build's skip_stages in docker config resolve ([@tj-smith47](https://github.com/tj-smith47))
* 125c8ae4094d align config-time env projection with the build planner ([@tj-smith47](https://github.com/tj-smith47))
* 99d994fc8d40 archives render the group's amd64 micro-arch variant ([@tj-smith47](https://github.com/tj-smith47))
* d27bd23a3dac centralize arch-token and variant-var policies; kill doubled mips filenames ([@tj-smith47](https://github.com/tj-smith47))
* d3b6b5c0d217 derive amd64 micro-arch variant at config time; cross-check all derived-name consumers ([@tj-smith47](https://github.com/tj-smith47))
* eb92234d8cc5 mark defaulted variant selectors in no-match publisher errors ([@tj-smith47](https://github.com/tj-smith47))
* b7b85dab8544 type amd64_variant as enum + model process-env RUSTFLAGS tiers ([@tj-smith47](https://github.com/tj-smith47))
* cb6297f8e822 type the whole amd64_variant config domain as the Amd64Variant enum ([@tj-smith47](https://github.com/tj-smith47))
* 225dea7b49c7 unify micro-arch variant seeding on the core SSOT #minor ([@tj-smith47](https://github.com/tj-smith47))
* 1b1c7b2d05d4 extend guard_no_unrendered backstop to winget/krew/nix/npm ([@tj-smith47](https://github.com/tj-smith47))
* d75fed20e89d finish the crate-universe conversion past dispatch into publisher bodies ([@tj-smith47](https://github.com/tj-smith47))
* cff96ec574d8 hard-fail on residual template delimiters before an irreversible publish ([@tj-smith47](https://github.com/tj-smith47))
* ae88cbf6adfd one crate-universe SSOT; registry gates + required/retain collapse see workspace crates ([@tj-smith47](https://github.com/tj-smith47))
* 10b1f9d7db76 shared rollback candidacy unstrands RollbackSkippedNoScope; one required-failure gate ([@tj-smith47](https://github.com/tj-smith47))
* 51dc140c41f4 test winget installer guard, reorder before clone, cover npm metapackage guard ([@tj-smith47](https://github.com/tj-smith47))
* e92bd4895edd derive prepare-skip, side-effect, and preflight stage sets from one source ([@tj-smith47](https://github.com/tj-smith47))
* ce623c92c3ac guard GitLab link-probe token exposure, GHES-aware GitHub probes, honest skip logging ([@tj-smith47](https://github.com/tj-smith47))
* 7e96e82ebdf5 require scheme match in GitLab probe host guard ([@tj-smith47](https://github.com/tj-smith47))
* 1947f74e94e3 finish the crate-universe conversion across every stage run loop ([@tj-smith47](https://github.com/tj-smith47))
* 90c5e69e8205 align preprocessor string scanning to the engine's raw boundary rule ([@tj-smith47](https://github.com/tj-smith47))
* f29d0d5d0e75 complete tera 2.0 migration — homebrew .0 index, preprocessor .N compat, date filter ([@tj-smith47](https://github.com/tj-smith47))
* b980399e29b6 fold shim boundary scan into string_lit; multiline go-control regexes; escape table pipes ([@tj-smith47](https://github.com/tj-smith47))
* 4c257a4cb17c restore date filter timezone= kwarg (tera 1.x parity) ([@tj-smith47](https://github.com/tj-smith47))
* 49497b3b32bd tri-state tool probe SSOT, one PATH-lookup primitive, token hints rendered from the env ladder ([@tj-smith47](https://github.com/tj-smith47))
* 91f68dd106ec single toml→yaml transcode route + shared discovery walk ([@tj-smith47](https://github.com/tj-smith47))
* 5a05d31b1e43 one RepoProbe->PreflightCheck mapper for both preflights ([@tj-smith47](https://github.com/tj-smith47))
* 05149b2bf99d single github_api_base resolver in core::http ([@tj-smith47](https://github.com/tj-smith47))
* 4ef8d97c9062 centralize operator lines, dockerhub username ladder, retriable-status rule, run-dir prefix ([@tj-smith47](https://github.com/tj-smith47))
* cb9612953a87 one forge upload driver; fix GitLab resume idempotency ([@tj-smith47](https://github.com/tj-smith47))

## [0.13.1] - 2026-07-02

### Bug Fixes

* 9b771a74891d drop AppDir scaffolding from dist; zero-pad zsync MTime ([@tj-smith47](https://github.com/tj-smith47))
* 4bb830ab20d2 pin .zsync MTime to SOURCE_DATE_EPOCH for determinism ([@tj-smith47](https://github.com/tj-smith47))
* dbcaafe12b4a strict-fail vanished/unmatched archive entries; propagate ELF+scan errors #133 ([@tj-smith47](https://github.com/tj-smith47))
* 6859d34782de stop silently masking write/mtime/stdin/IO failures #134 ([@tj-smith47](https://github.com/tj-smith47))
* f44720f56268 thread allow_http through ClientOptions so disable_ssl endpoints work ([@tj-smith47](https://github.com/tj-smith47))
* a000fd2d767b size hdiutil image explicitly to prevent spurious ENOSPC ([@tj-smith47](https://github.com/tj-smith47))
* b7bbd986b35e derive curl-sh asset names from the engine, not hardcoded shell ([@tj-smith47](https://github.com/tj-smith47))
* cb1aa93565a6 declare the configured formatter binary in publisher requirements ([@tj-smith47](https://github.com/tj-smith47))
* f7fbbc6d637f stop cargo dry-run spawn tests flaking on ETXTBSY #129 ([@tj-smith47](https://github.com/tj-smith47))
* 013aa262affb cloudsmith unverifiable-checksum uploads; npm token/extra-file errors surface #132 ([@tj-smith47](https://github.com/tj-smith47))
* d23cbe334ae3 gate ALL one-way doors on a required failure, not just Submitter #F1 ([@tj-smith47](https://github.com/tj-smith47))
* 1bafb9503234 make blob rollback reachable via dedicated rollback_publishers ([@tj-smith47](https://github.com/tj-smith47))
* cb9e449df5ad nix ELF inspection failure must fail the publish, not drop autoPatchelfHook #133 ([@tj-smith47](https://github.com/tj-smith47))
* 7e91f26c2d94 render templated secret_name for dockerhub and gemfury ([@tj-smith47](https://github.com/tj-smith47))
* 1fccbe0c189b upload blob assets before the one-way-door publishers ([@tj-smith47](https://github.com/tj-smith47))
* 0eef96a22523 fail on unreadable checksum artifact; warn on write-path template fallback #131 ([@tj-smith47](https://github.com/tj-smith47))
* 9d4305869cbe propagate config+workspace load errors, never cut a wrong version silently #130 ([@tj-smith47](https://github.com/tj-smith47))
* 9b2f088e378e self-skip when github-release is deselected ([@tj-smith47](https://github.com/tj-smith47))
* 2811e400e06c eliminate failure-hiding across the repo (stubs, swallowed errors, auto-pass) ([@tj-smith47](https://github.com/tj-smith47))
* c757ec4d4c08 centralize three drifting patterns onto core types ([@tj-smith47](https://github.com/tj-smith47))
* fe804e899de8 single-source the ELF dynamic-linking probe, fix class-gate divergence #136 ([@tj-smith47](https://github.com/tj-smith47))
* df57ff9a89fa make StageId the bidirectional stage-vocabulary SSOT ([@tj-smith47](https://github.com/tj-smith47))
* f00908967618 single-source skip-upload log, token rollback, poll eligibility ([@tj-smith47](https://github.com/tj-smith47))
* efc0e586166b single-source the GitHub-publisher preflight loop and gh probe ([@tj-smith47](https://github.com/tj-smith47))
* 588a30ca06bb centralize octocrab transport-error classification ([@tj-smith47](https://github.com/tj-smith47))
* be134cdc2060 single-source dist sidecar basenames and the run-<id> dir ([@tj-smith47](https://github.com/tj-smith47))

## [0.13.0] - 2026-06-27

### Features

* d14eeca0e63f exclude glob filter to drop assets per destination ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* a8b40e4b7b92 align Warning/Error/Note with the enclosing header, not body depth ([@tj-smith47](https://github.com/tj-smith47))
* d33dc3cc7f8d root-cause v0.12.3 publish hang + signing/upload UX ([@tj-smith47](https://github.com/tj-smith47))
* c97861d281e2 bound every remote subprocess and HTTP client against indefinite hangs ([@tj-smith47](https://github.com/tj-smith47))

## [0.12.3] - 2026-06-26

### Bug Fixes

* 94c4a1a32d5f bound the stage so a slow notification can never fail a published release ([@tj-smith47](https://github.com/tj-smith47))
* c3a4b7854ad1 only synthesize default --bin when a bin named after the crate exists #51 ([@tj-smith47](https://github.com/tj-smith47))
* ba075f01b083 kill the whole process subtree on a windows timeout ([@tj-smith47](https://github.com/tj-smith47))
* fabad9f2268d land the cargo group bump — provider, rsa vuln, schemars/toml/rcgen ([@tj-smith47](https://github.com/tj-smith47))
* f45b93b77bf5 pin lifecycle-script mtime so signed apk packages are reproducible ([@tj-smith47](https://github.com/tj-smith47))

## [0.12.2] - 2026-06-25

### Bug Fixes

* 3b6a859203ea exclude a cancelled determinism shard from the publish gate ([@tj-smith47](https://github.com/tj-smith47))
* 7dbfc22f759b notify must not require a clean tree or a tag at HEAD ([@tj-smith47](https://github.com/tj-smith47))
* 54dc39f5732e patch RUSTSEC advisories; add cargo-audit gate + dependabot ([@tj-smith47](https://github.com/tj-smith47))
* 55fe9c71e646 stop chocolatey 403 force-retry; surface the real registry error ([@tj-smith47](https://github.com/tj-smith47))
* d6f43db359fb github-release auto-resumes a stale leftover draft ([@tj-smith47](https://github.com/tj-smith47))
* e5cbdce13768 name the before-hook in logs; run env-preflight before hooks ([@tj-smith47](https://github.com/tj-smith47))
* bff0f192a82a push the revert commit before deleting the tag on rollback ([@tj-smith47](https://github.com/tj-smith47))

## [0.12.1] - 2026-06-24

### Bug Fixes

* 5cd973d4a46b publish-only log noise, hangs, dup artifacts, nsis reproducibility #patch ([@tj-smith47](https://github.com/tj-smith47))
* 9b2a2782daee pass bash ${#...} through Tera rendering like GoReleaser ([@tj-smith47](https://github.com/tj-smith47))

## [0.12.0] - 2026-06-22

### Features

* 9320c98c1156 ship per-arch macOS/Windows installers via shard routing with deterministic MSI ProductCode #minor ([@tj-smith47](https://github.com/tj-smith47))
* 66719950e3a3 cargo content-vs-version poison guard prevents re-publishing divergent crates ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 54787807d24a provision nfpm in-repo for the linux determinism test, fail-loud not skip ([@tj-smith47](https://github.com/tj-smith47))
* e8e9a4017ae1 gate installer stages by PATH-existence, not --version exit ([@tj-smith47](https://github.com/tj-smith47))
* eb0ff7615fdf wrap webhook body in a valid JSON envelope ([@tj-smith47](https://github.com/tj-smith47))
* 45d82f87d5ed cargo poison guard fails closed when content identity is unverifiable ([@tj-smith47](https://github.com/tj-smith47))
* f9d94ac8d9be never read a macOS .app bundle directory as a file ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* 1b42c5e50159 Revert "chore(release): bump workspace → 0.12.0" ([@tj-smith47](https://github.com/tj-smith47))
* 9ebc6f14838e Revert "chore(release): bump workspace → 0.12.0" ([@tj-smith47](https://github.com/tj-smith47))
* 98e3a5eaf500 Revert "chore(release): bump workspace → 0.12.0" ([@tj-smith47](https://github.com/tj-smith47))
* f5899b853b0d Revert "chore(release): bump workspace → 0.12.0" ([@tj-smith47](https://github.com/tj-smith47))

## [0.11.3] - 2026-06-19

### Bug Fixes

* 21462094b585 scope schema validation to the selected publisher surface #none ([@tj-smith47](https://github.com/tj-smith47))
* 542717fa7a81 gate signing-key preflight on resolved publisher surface #none ([@tj-smith47](https://github.com/tj-smith47))

## [0.11.2] - 2026-06-18

### Bug Fixes

* 9fcf93cfabc6 scope --publishers to its true surface; close custom-publisher allowlist escape ([@tj-smith47](https://github.com/tj-smith47))
* d79b4efbfb68 tighten release-pipeline default output to header+RESULT ([@tj-smith47](https://github.com/tj-smith47))

## [0.11.1] - 2026-06-18

### Bug Fixes

* 7415e789d825 route per-crate no-op skips through skip_line so default output is clean ([@tj-smith47](https://github.com/tj-smith47))
* f0f0a718678c honor --publishers/--skip publisher selection in env requirement collection ([@tj-smith47](https://github.com/tj-smith47))
* 38e54b15bb0c restore skip-output tests under --show-skipped; widen sign artifact-filter ([@tj-smith47](https://github.com/tj-smith47))

## [0.11.0] - 2026-06-17

### Features

* a77311e95c11 --publishers allowlist + GR-parity skip vocab (homebrew/chocolatey canonical) + deselect observability ([@tj-smith47](https://github.com/tj-smith47))
* aa051889ed18 core foundation for per-publisher selection ([@tj-smith47](https://github.com/tj-smith47))
* f83e607adb2e uniform per-publisher --skip/--publishers filter at dispatch ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 61f1e9b3df55 continue honors and validates --publishers / --skip selectors ([@tj-smith47](https://github.com/tj-smith47))
* 07a55d666c8a tighten check-config --publishers to configured set + unify help + strengthen accept tests ([@tj-smith47](https://github.com/tj-smith47))
* af5972956f69 disable rollback in the hermetic harness child ([@tj-smith47](https://github.com/tj-smith47))
* daf08c6c1f94 export cosign key as a path, not only contents ([@tj-smith47](https://github.com/tj-smith47))
* 489861307239 skip keyless cosign in the harness (no ambient OIDC) ([@tj-smith47](https://github.com/tj-smith47))
* de308e1f6137 demote subprocess command echoes to verbose across all stages ([@tj-smith47](https://github.com/tj-smith47))
* d36692cbd8be demote subprocess-command echo to verbose; keep concise default results ([@tj-smith47](https://github.com/tj-smith47))
* a48412794798 skip a zero-match config under restricted builds ([@tj-smith47](https://github.com/tj-smith47))
* 6b9469dd0793 seal env in two auth/provenance tests to kill ambient-env flake ([@tj-smith47](https://github.com/tj-smith47))
* 47dc025d6524 address T2 review — poller-deselect test, real non-invocation proof, simplify winget guard, log sibling skips ([@tj-smith47](https://github.com/tj-smith47))
* 4419b14d6661 gate out-of-dispatch publish stages on --publishers allowlist ([@tj-smith47](https://github.com/tj-smith47))
* 4e107f773580 govern announce stage by the publisher allowlist ([@tj-smith47](https://github.com/tj-smith47))
* bdeee1a91b84 fold deselect gate into AnnounceDecision ([@tj-smith47](https://github.com/tj-smith47))
* fe9af0bf9dbc DRY publisher-selection validation + doc should_skip ([@tj-smith47](https://github.com/tj-smith47))
* e5234a05b412 use sealed_env() for hermetic-env publish tests ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* 818e2582c30b normalize dry-run indentation to one nesting level ([@tj-smith47](https://github.com/tj-smith47))

## [0.10.0] - 2026-06-15

### Features

* e590ff1923b7 complete published-crate metadata for crates.io parity ([@tj-smith47](https://github.com/tj-smith47))
* 700abc99eb1e warn when an announce template references a secret-named env var ([@tj-smith47](https://github.com/tj-smith47))
* c34dce89f356 add keep_versions retention pruning ([@tj-smith47](https://github.com/tj-smith47))
* 3b570db91705 auto-inject OCI image labels (deterministic created) + derive nfpm vendor ([@tj-smith47](https://github.com/tj-smith47))
* 552b6bb121c6 build app_bundle/dmg/pkg/msi/nsis on Linux CI (unsigned); base-image tooling ([@tj-smith47](https://github.com/tj-smith47))
* aa38db2368c6 genuine before_publish gate, uploads, custom publisher; fix if_condition rustdoc ([@tj-smith47](https://github.com/tj-smith47))
* 24630a7abc54 cask livecheck support; dogfood via cask, drop dual-publish formula ([@tj-smith47](https://github.com/tj-smith47))
* 09fbb5d898dc add --raw to send messages without Tera rendering (gate the provider-side render too) ([@tj-smith47](https://github.com/tj-smith47))
* 9bea42ce2d66 redact secrets in outbound body by default; --allow-secrets opt-out ([@tj-smith47](https://github.com/tj-smith47))
* 23ac370305a3 per-package auth mode (auto/token/oidc) with OIDC fallback ([@tj-smith47](https://github.com/tj-smith47))
* 491930a2dbd8 tokenless Trusted Publishing via GitHub OIDC; enable npms dogfood ([@tj-smith47](https://github.com/tj-smith47))
* 0298ae3e4e1a refuse to publish snapshot/dev/0.0.0 versions ([@tj-smith47](https://github.com/tj-smith47))
* 2b55ecf3d976 wire generic HTTP upload stage (config.uploads was dead config) ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 3abd9c0121e1 map arch correctly (no x86_64 relabel); install LICENSE/man/completions; license array; derive source arch ([@tj-smith47](https://github.com/tj-smith47))
* c6cbd5e7cf95 per-crate metadata in source PKGBUILD; warn-skip unrepresentable scoop arch; document nfpm vendor ([@tj-smith47](https://github.com/tj-smith47))
* 1e68db37ae06 emit metadata on the publish-only path so cargo binstall fetches prebuilt ([@tj-smith47](https://github.com/tj-smith47))
* 3bb1ef270621 scope command-line-utilities to the cli; per-crate keywords for platform stages ([@tj-smith47](https://github.com/tj-smith47))
* 28d72e01548c mailmap-driven login back-fill across author aliases ([@tj-smith47](https://github.com/tj-smith47))
* efb370f60166 license expression + real LICENSE url; route install by artifact type (msi/nsis/zip); add projectSourceUrl/bugTrackerUrl ([@tj-smith47](https://github.com/tj-smith47))
* a421129999be select installer artifact by format so use:msi/nsis can't cross-wire ([@tj-smith47](https://github.com/tj-smith47))
* ec4645f85622 unblock v0.10.0 — coverage floor 92.5, macOS pkg test gate, doc anchors ([@tj-smith47](https://github.com/tj-smith47))
* 8e9beb04d68f hard-fail empty maintainer for deb/apk; emit Artifactory deb matrix params + Cloudsmith distribution so debs index ([@tj-smith47](https://github.com/tj-smith47))
* ec6c81f31efe reject empty/unknown deb matrix slugs; require maintainer for ipk ([@tj-smith47](https://github.com/tj-smith47))
* 9a1480d499a6 validate deb matrix-param slugs; require maintainer only when a deb/apk is actually built ([@tj-smith47](https://github.com/tj-smith47))
* 1ff12c19d8fa platform-aware msi/pkg tool gates; add srpm to ubuntu shard ([@tj-smith47](https://github.com/tj-smith47))
* aee518b2f885 restore *.pkg allowlist tuple; honest reproducibility comment ([@tj-smith47](https://github.com/tj-smith47))
* 8d4ba7f6dc5b wire Metadata.Documentation template var; explicit per-crate no-leakage assertion ([@tj-smith47](https://github.com/tj-smith47))
* c44627d79968 un-skip dogfood bundle; dedup collapsing arches; absolutize bundle path ([@tj-smith47](https://github.com/tj-smith47))
* 0fee01001ca4 emit every os/arch in cask, count casks, retire stale formula ([@tj-smith47](https://github.com/tj-smith47))
* 9b9920a113e9 install completions/manpages + livecheck + test block in formula; render dual-license via any_of ([@tj-smith47](https://github.com/tj-smith47))
* 9c837c669655 unleak completions doc link; warn on ignored livecheck opt-in ([@tj-smith47](https://github.com/tj-smith47))
* a6e4b6e19beb emit per-platform files: (binary + LICENSE/README) so nothing is dropped; validate shortDescription length ([@tj-smith47](https://github.com/tj-smith47))
* fb6e6c1d74de exclude CHANGELOG.md and dedup LICENSE.md from files: list ([@tj-smith47](https://github.com/tj-smith47))
* 1ed53cf49a57 collapse blob/cloudsmith per-file upload firehose to a summary ([@tj-smith47](https://github.com/tj-smith47))
* 44a3b812b928 finish skip-wording and arrow-glyph uniformity sweep ([@tj-smith47](https://github.com/tj-smith47))
* 21d6622c360b polish six user-facing log-quality issues ([@tj-smith47](https://github.com/tj-smith47))
* 67cd8d72af34 uniform stage-header indent; coherent retry warn + closing line ([@tj-smith47](https://github.com/tj-smith47))
* 62cd6f484415 drop dead strict-license resolver; validate maintainer handles; correct license doc ([@tj-smith47](https://github.com/tj-smith47))
* 7d1aa2f33492 emit each meta.platforms entry once; dedup archives by nix system ([@tj-smith47](https://github.com/tj-smith47))
* bbea7e1a6246 emit meta.maintainers/changelog/longDescription, license list with lib.licenses mapping, install completions+man ([@tj-smith47](https://github.com/tj-smith47))
* d2c2fd4f666e repair postinstall ReferenceError; derive description/homepage/license/author from Cargo.toml; add files/engines/provenance ([@tj-smith47](https://github.com/tj-smith47))
* 3ae18fbae378 byte-reproducible xar TOC; appbundle copy; shared symlink-safe dir-copy; installer PATH ([@tj-smith47](https://github.com/tj-smith47))
* 0019f0063906 blob workspace targets, announcer doc, guard test/doc ([@tj-smith47](https://github.com/tj-smith47))
* 2784bc5b0c79 enforce non-release guard in blob + announce stages ([@tj-smith47](https://github.com/tj-smith47))
* d193c6c9ac15 harden residual-delimiter guard; render cask service/app; gate snapcraft pre-publish ([@tj-smith47](https://github.com/tj-smith47))
* 39eac0b5a9bd render user-templated config fields in all manifest generators ([@tj-smith47](https://github.com/tj-smith47))
* 640ea67fa873 stop blocking -dev pre-releases; pin guard wiring ([@tj-smith47](https://github.com/tj-smith47))
* fc797e138bf6 match docs lint sample to real output; dedupe gh-stderr redaction onto with_env ([@tj-smith47](https://github.com/tj-smith47))
* 4ff4ac08952e key catalog identity on fileMatch, not name ([@tj-smith47](https://github.com/tj-smith47))
* 1e77fd8b938a label NoOp refresh as Update; key versions carry-forward on fileMatch ([@tj-smith47](https://github.com/tj-smith47))
* 579acb917c0d emit extract_dir, checkver, and autoupdate for bucket-ready manifests ([@tj-smith47](https://github.com/tj-smith47))
* ee76499f2f10 harden sidecar-suffix derivation; truthful checksum-algorithm doc; drop dead legacy path ([@tj-smith47](https://github.com/tj-smith47))
* eeabec7b7fa6 derive license like every other publisher; tidy license follow-ups ([@tj-smith47](https://github.com/tj-smith47))
* 4893e15fee0b resolve extra_files specs (GR parity); correct error label; doc/comment cleanups ([@tj-smith47](https://github.com/tj-smith47))
* efe38e3e2df1 correct Moniker, default UpgradeBehavior, add Documentations + InstallerSwitches ([@tj-smith47](https://github.com/tj-smith47))
* 0922ad71be13 address review findings ([@tj-smith47](https://github.com/tj-smith47))
* 581d52f8622a use house SPDX parser for licenseUrl suppression ([@tj-smith47](https://github.com/tj-smith47))
* 5382501fa528 rename default push-token env var FURY_TOKEN to FURY_PUSH_TOKEN ([@tj-smith47](https://github.com/tj-smith47))
* 1c9c7095047d use shared http::blocking_client for existence probe; test encode_package_path ([@tj-smith47](https://github.com/tj-smith47))
* aa9746571ab5 share rollback-target collection between artifactory + uploads ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* b7381c77ecbf dual-license MIT OR Apache-2.0; single-source derivable metadata ([@tj-smith47](https://github.com/tj-smith47))
* fa39bc6fd78d document outbound secret redaction, --allow-secrets, and the check-config lint ([@tj-smith47](https://github.com/tj-smith47))

## [0.9.1] - 2026-06-13

### Features

* ce8ce6034072 consolidated run helper with verbose live-stream + emit-on-failure ([@tj-smith47](https://github.com/tj-smith47))
* 5a32ab7bf996 route per-crate no-config skips to debug; add --show-skipped ([@tj-smith47](https://github.com/tj-smith47))
* 75da7615ca30 proactive GitHub upload pace + secondary-RL exhaustion proof ([@tj-smith47](https://github.com/tj-smith47))

---
### Bug Fixes

* 784d18178ddb ship a runnable musl binary in the apk package ([@tj-smith47](https://github.com/tj-smith47))
* 52869be8b97d kill recursive sidecar chains via primary-subject taxonomy ([@tj-smith47](https://github.com/tj-smith47))
* bebbde927855 bind every build-consuming Linux surface to the gnu build ([@tj-smith47](https://github.com/tj-smith47))
* 1be521c3eb4f route live tee to stderr, concurrent stdin, dedup stream methods ([@tj-smith47](https://github.com/tj-smith47))
* 5d3a37425a54 sign by digest, never by a movable tag ([@tj-smith47](https://github.com/tj-smith47))
* 82639a0428da suppress false nothing-pushed warning on cask-only configs ([@tj-smith47](https://github.com/tj-smith47))
* 06e33eb62c64 make derivation formatting mandatory and fail loud, no unformatted push ([@tj-smith47](https://github.com/tj-smith47))
* 1a590e37ce4d parity — artifacts:all signs the combined checksums file ([@tj-smith47](https://github.com/tj-smith47))
* 6f77bf4ae887 restore GR parity — sign every Checksum kind, not combined-only ([@tj-smith47](https://github.com/tj-smith47))
* 28d100d1c270 surface verify-release findings in the end-of-release Summary ([@tj-smith47](https://github.com/tj-smith47))
* cda1eb7bd14d label container-start failures and anchor the smoke marker ([@tj-smith47](https://github.com/tj-smith47))
* 52e55ee29d64 make install smoke-test failures diagnosable ([@tj-smith47](https://github.com/tj-smith47))

---
### Others

* cd87602f30ed document recursion detector's name-suffix assumption ([@tj-smith47](https://github.com/tj-smith47))
* 9786400ff5a3 drop vestigial VerifyReleaseSummary.ran field ([@tj-smith47](https://github.com/tj-smith47))
* a08507ceafa3 pin Debug-verbosity cell of no-config skip matrix ([@tj-smith47](https://github.com/tj-smith47))
* 003776f7eb45 make upload_pace_zero_is_a_no_op deterministic (relative pacing compare) ([@tj-smith47](https://github.com/tj-smith47))
* 2fc21f14abe4 cover docker-sign digest-pin edge cases and the missing-digest path ([@tj-smith47](https://github.com/tj-smith47))
* f3761b63cdbf fix Windows fake-cosign arg capture in docker-sign digest test ([@tj-smith47](https://github.com/tj-smith47))

## [0.9.0] - 2026-06-11

### Bug Fixes

* 9ae4ed392be8 inject Is* template vars and NightlyBuild as typed bools/number (TJ Smith)
* decfe86b6618 review fixes — unset eviction, NightlyBuild truthiness note, test typing fidelity (TJ Smith)
* a6be5873b787 always write run summary; gate rollback on publish state (TJ Smith)
* ad49ce3fb5c7 review fixes — summary clobber guard, probe fail-closed, kms PATH seam (TJ Smith)
* 859e2f5860cf reject multi-document typed configs in builtin mode too (TJ Smith)
* bc2e553ead8c stop replacing PATH wholesale in spawn-failure tests (TJ Smith)
* fb7e5a166ad1 fail on missing expected signature/SBOM assets (TJ Smith)
* 9157a4919ff1 pin install-smoke containers to the package arch; drop apk self-provides (TJ Smith)
* 7634ac80354a re-review fixes — transitive ids verdict for derived subjects, typed multi-doc pin, docker_signs warning (TJ Smith)
* f79b7ebc5089 review fixes — resolved-name filter keying, probe pinning, probe diagnostics, lock recovery (TJ Smith)
* 727284f7957e review fixes — sbom derivation equivalence, release.ids sig inheritance (TJ Smith)

---
### Others

* cf02d17fcae2 rollback v0.8.0 [skip ci] (anodize-rollback)
* b73a16855234 "chore(release): rollback v0.8.0 [skip ci]" (TJ Smith)

## [0.8.0] - 2026-06-11

### Features

* 6a628185b1ba add --allow-rerun flag to anodizer publish (TJ Smith)
* cece142186c7 config-declarable on_error/on_rollback hooks (TJ Smith)
* 9e27a01471c9 expose failure-hook context as ANODIZER_* env vars (TJ Smith)
* 3eca978cea09 gate all irreversible publishers on any required failure (TJ Smith)
* 8f17ca07b47d preflight guard for publish-set dependency completeness (TJ Smith)
* e1c4c083e2de retain_on_rollback, on_error hooks, anodizer notify (TJ Smith)
* bdef8a2f957c universal publisher idempotency for safe re-runs (TJ Smith)
* a6665ddb2daf explicit --version override for autotag (TJ Smith)
* ca8155a6fef5 auto-detect dind for install_smoke; wire blobs to MinIO (TJ Smith)

---
### Bug Fixes

* c4031cd1a757 set ArtifactName from archive filename in url_template render (TJ Smith)
* 6e2b2387e422 unique SSH key filename per clone to prevent EEXIST on retry (TJ Smith)
* 39523204f1fb gate zigbuild routing on a reachable zig toolchain (TJ Smith)
* 173d8b4318b4 route host linux-gnu builds through zigbuild for a hermetic glibc floor (TJ Smith)
* ecc74f794f74 make republish_in_moderation actually re-push (TJ Smith)
* 3beec78e7866 add on_error to PublishDefaults with append-merge semantics; wire retain_on_rollback on cargo, schemastore, mcp (TJ Smith)
* 83da4f3add02 correct submitter required-gating warning text (TJ Smith)
* c0e19c2df6da durability fixes W1-W3, F1-F2, S1-S2, F3, GHA#1-2, #58-59 (TJ Smith)
* 532096abd458 error loudly when artifact URL absent in publish mode; tolerate in snapshot (TJ Smith)
* a29bb53a6a91 guarantee trailing newline on written SSH key (TJ Smith)
* cef07f0a4861 key the workspace-root dep cache by resolved root path (TJ Smith)
* 6e1980a40a5e propagate render errors in AUR rollback creds + add render tests (TJ Smith)
* 7e63afe5b5c5 redact custom header values and target URL in artifactory dry-run log (TJ Smith)
* 2a22c30de1f9 rehydrate sha256 via ChecksumStage in publish/continue pipeline (TJ Smith)
* 4de628a767e3 render npm registry/tag/metadata and dockerhub username templates (TJ Smith)
* 84f84dd40efd render secret/url/branch/token config templates before use (TJ Smith)
* e236fb1782ac resolve package renames in publish-set preflight (TJ Smith)
* 17bc6953a4dc resolve renames in the dependency wait gate (TJ Smith)
* 343d4f6c2a2b require all live publishers and restore install_smoke (TJ Smith)

---
### Others

* 4bd2c68379cd correct on_error timing and RolledBack semantics (TJ Smith)
* 099797b072ae DepEntry struct, alias in guard errors, shared root cache (TJ Smith)
* e1f5b88d14bd single-source failure vars; pin env-channel exhaustiveness (TJ Smith)
* 5c24cb2dfdc7 normalize hook output path for Windows in on_error test (TJ Smith)
* b9ad18ff3708 verify retain_on_rollback skips rollback dispatch (TJ Smith)
* 14d788e6014c test+fix: address v0.8.0 review findings (TJ Smith)

## [0.6.0] - 2026-06-08

Changes since `v0.5.0`. Will be cut as the next release.

### Features

- **`anodize tag rollback`** — new subcommand that deletes anodize-managed
  tags at a SHA and reverts (or resets past) the bump commit they point at.
  Failure-recovery counterpart to `anodize tag`. Flags: `--dry-run`,
  `--no-push`, `--scope={all,lockstep,per-crate}`,
  `--mode={revert,reset}`, `--branch <name>`. SHA-derived branch
  resolution is race-immune to default-branch movement. Safety check
  hard-fails when non-bump commits sit between HEAD and the target SHA
  under `--mode=revert`. (`3a27f92`, `5948253`, `ba81b6e`, `41947cb`)
- **`docker_v2:` graduates to canonical Docker API** with full GoReleaser
  v2.16 surface — Platforms metadata, pre/post hook contract
  (`{{ .Images }}` / `{{ .Dockerfile }}` / `{{ .ContextDir }}` /
  `{{ .Digest }}` / `{{ .BaseImage[Digest] }}`), podman backend (Linux-only),
  cleaner `images` + `tags` separation. Legacy `dockers:` block is now
  rejected at config-load time with a migration error. (`166e3a7`,
  `9e6f452`, `dbc87b7`)
- **Anodizer publishes itself as an MCP server.** The repo's own
  `.anodizer.yaml` declares `mcp:` + per-crate `docker_v2:`; the
  distroless OCI image at `ghcr.io/tj-smith47/anodizer:<version>` carries
  `ENTRYPOINT ["/usr/local/bin/anodizer"]` + `CMD ["mcp"]` so MCP clients
  `docker run` it as a stdio server. (`596e1a3`, `41947cb`)
- **Per-crate workspace-aware tag** — `anodizer tag` dispatches per-crate
  in workspaces with per-crate `[package].version`, emits `crates` (JSON
  array) and `versions` (JSON object) step outputs, propagates bumps to
  intra-workspace `path + version` dep specs. (`7735448`, `475109e`,
  `ba82aa1`, `0135f56`)
- **Per-crate dist subdir layout for workspace release** —
  `release --publish-only` consumes `preserved-dist/<crate>/` subdirs
  emitted by per-crate determinism shards. (`9c13daf`, `76cb613`,
  `9562bc3`)
- **`publishers[].required:` field** — every publisher accepts a
  `required:` boolean that wires through `resolved_required()` so the
  release pipeline knows whether a publisher's failure should block the
  Submitter gate / non-zero exit. Submitter-group publishers (cargo,
  chocolatey, winget, snapcraft, upstream-AUR) warn loudly when set to
  `true` since their failure cannot be recovered. (`a90f8ac`, `948dd4a`,
  `7de69a4`, `d035aaf`)
- **`if:` template-conditional gates** across publishers, hooks,
  announcers, archives, blob entries — when the rendered result is falsy
  (`"false"` / `"0"` / `"no"` / empty), the entry is skipped. Render
  failure hard-errors. (`10af9cf`)
- **AI release-note enhancement** — `changelog.use: ai` wires
  anthropic / openai / ollama as backends; produces a polished release
  note from the raw commit log. (`c8342c5`)
- **GoReleaser v2.16 parity** — nightly `tag_name:` templates, srpm
  Format/Ext overrides, immutable releases policy, `homebrew_casks:` as
  the canonical Homebrew surface (deprecated `publish.homebrew`),
  v2.12.6→v2.15.3 deprecation aliases for renamed fields. (`f9ec8d5`,
  `63bc5fc`, `1868af6`, `d0aff91`)
- **Pre-publishing hooks** (`before_publish:`) and per-artifact iteration
  with `ids` / `artifacts` filters. (`2e55c3f`, `a94ab91`)
- **Recursive config includes**, strict `template_vars:`, `meta_`
  propagation. (`42eb1ff`)
- **npm + gemfury publishers** — full implementation with idempotency
  probe, retry, templated extra files, rollback (npm) and `furies:`
  alias (gemfury). (`e3d7264`, `94e139d`, `2335dae`)
- **Single-target build, split/merge, nightly builds** audit closures —
  scheduled nightly workflow, version_template, keep_single_release with
  safety + dry-run visibility. (`aa11201`, `bc35263`, `b314c59`,
  `35e8d31`)
- **Per-publisher Pre-image SHA tracking** for `KrewExtra::bot_template_pre_image_shas`
  — rollback drift-detection for krew bot-template mode (Unchanged /
  Drifted / Missing / Unreadable). (`5948253`)
- **`actions: read` permission** required when the release job downloads
  artifacts from a sibling workflow (`from-artifact: anodizer-linux`,
  cross-workflow patterns). (workflow hardening)

### Fixes

- **Unblock cfgd release** — `--publish-only` resume_release auto-enable,
  per-iteration skip_stages propagation from `workspaces[].skip`,
  per-crate manifest path re-anchor, OCI `version` field omitted from the
  wire, BotTemplate pre-image SHA recording, cargo intra-workspace dep
  pin propagation. (`596e1a3`, `58b4e7a`, `76e766f`, `aec8eef`,
  `7f26c9f`, `6ca21a9`)
- **Source archive: extra-files mode normalized to 0o644 under SDE** for
  cross-OS determinism. (`c224627`)
- **Tag bump commits omit `[skip ci]` on the primary commit** so the
  tag-push trigger fires downstream `release.yml`. Side-effect
  `version_sync` propagation commits still carry the marker. (`a4d55d5`)
- **`wait_for_workspace_deps` gate** prevents cross-crate publish race
  during topo-ordered workspace publish. (`f756834`)
- **Detached-HEAD push** — `git push HEAD via refs/heads/<branch>`
  refspec, resolve detached HEAD before push. (`292af2d`, `68de654`)
- **Per-crate bump idempotent** when manifests are already at the target
  version. (`0135f56`)
- **`.anodizer.yaml workspaces:` takes precedence** over
  `[workspace.package].version` — authoritative signal for
  per-crate-with-grouping intent. (`e6a9ee9`)
- **`check`: fall back to `GITHUB_REF_NAME`** for tag_override when
  triggered by tag push. (`4b8d5c8`)
- **Audit follow-ups** drained across B1–B24 — pkg, msi, nsis, dmg,
  appbundle, changelog, build, release, git, docker, publish modules.

### CI / Workflows

- **Switched to cargo-nextest + sccache** layered atop rust-cache for
  faster CI. (`7d5573e`)
- **Scheduled nightly workflow** with date-based versioning. (`35e8d31`)
- **Sharded determinism matrix** (Linux + macOS + Windows-x86_64 +
  Windows-aarch64) — each shard validates only its own targets;
  cross-shard hash comparison is intentionally relaxed.
- **`Rollback on release failure` step** — workflow integrates
  `anodizer tag rollback "$GITHUB_SHA"` as the
  `if: (failure() || cancelled())` recovery hook.

### Docs

- New [Release Workflow Strategies](docs/site/content/docs/ci/release-workflows.md)
  page covering single-crate / lockstep / per-crate / hybrid / split-CI
  shapes with the decision tree. (`ef17e7d`)
- New `## crates[].docker_v2`, `## crates[].publish.krew`, `## mcp`
  schema sections in the auto-generated configuration reference.
- `tag rollback` documented in README + release-resilience guide +
  auto-tagging guide.
- `_preserved-bin/` layout documented in the determinism guide.
- `docker_v2:` page rewritten end-to-end; legacy `dockers:` references
  removed across packages, retry, dogfooding, and CI docs.
- MCP registry page: "Wiring the OCI image" subsection added.
- Krew page: "Rollback semantics for bot-template mode" + graceful
  degradation note for `project_root` auto-detect.
- anodizer-action page: 7 previously-undocumented inputs added
  (`apk-private-key`, `preserve-dist`, `shard-label`, `determinism`,
  `determinism-runs`, `determinism-stages`, `determinism-targets`);
  retry behavior callout updated to flag stateful
  `--publish-only` / `--rollback-only` / `tag rollback`.

[Unreleased]: https://github.com/tj-smith47/anodizer/compare/v0.30.0...HEAD
[0.30.0]: https://github.com/tj-smith47/anodizer/compare/v0.29.0...v0.30.0
[0.29.0]: https://github.com/tj-smith47/anodizer/compare/v0.28.1...v0.29.0
[0.28.1]: https://github.com/tj-smith47/anodizer/compare/v0.28.0...v0.28.1
[0.28.0]: https://github.com/tj-smith47/anodizer/compare/v0.27.0...v0.28.0
[0.27.0]: https://github.com/tj-smith47/anodizer/compare/v0.26.0...v0.27.0
[0.26.0]: https://github.com/tj-smith47/anodizer/compare/v0.25.2...v0.26.0
[0.25.2]: https://github.com/tj-smith47/anodizer/compare/v0.25.1...v0.25.2
[0.25.1]: https://github.com/tj-smith47/anodizer/compare/v0.25.0...v0.25.1
[0.25.0]: https://github.com/tj-smith47/anodizer/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/tj-smith47/anodizer/compare/v0.23.0...v0.24.0
[0.23.0]: https://github.com/tj-smith47/anodizer/compare/v0.22.2...v0.23.0
[0.22.2]: https://github.com/tj-smith47/anodizer/compare/v0.22.1...v0.22.2
[0.22.1]: https://github.com/tj-smith47/anodizer/compare/v0.20.0...v0.22.1
[0.20.0]: https://github.com/tj-smith47/anodizer/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/tj-smith47/anodizer/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/tj-smith47/anodizer/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/tj-smith47/anodizer/compare/v0.16.1...v0.17.0
[0.16.1]: https://github.com/tj-smith47/anodizer/compare/v0.16.0...v0.16.1
[0.16.0]: https://github.com/tj-smith47/anodizer/compare/v0.15.5...v0.16.0
[0.15.5]: https://github.com/tj-smith47/anodizer/compare/v0.15.4...v0.15.5
[0.15.4]: https://github.com/tj-smith47/anodizer/compare/v0.15.3...v0.15.4
[0.15.3]: https://github.com/tj-smith47/anodizer/compare/v0.15.1...v0.15.3
[0.15.1]: https://github.com/tj-smith47/anodizer/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/tj-smith47/anodizer/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/tj-smith47/anodizer/compare/v0.13.1...v0.14.0
[0.13.1]: https://github.com/tj-smith47/anodizer/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/tj-smith47/anodizer/compare/v0.12.3...v0.13.0
[0.12.3]: https://github.com/tj-smith47/anodizer/compare/v0.12.2...v0.12.3
[0.12.2]: https://github.com/tj-smith47/anodizer/compare/v0.12.1...v0.12.2
[0.12.1]: https://github.com/tj-smith47/anodizer/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/tj-smith47/anodizer/compare/v0.11.3...v0.12.0
[0.11.3]: https://github.com/tj-smith47/anodizer/compare/v0.11.2...v0.11.3
[0.11.2]: https://github.com/tj-smith47/anodizer/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/tj-smith47/anodizer/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/tj-smith47/anodizer/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/tj-smith47/anodizer/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/tj-smith47/anodizer/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/tj-smith47/anodizer/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/tj-smith47/anodizer/compare/v0.6.0...v0.8.0
[0.6.0]: https://github.com/tj-smith47/anodizer/compare/v0.5.0...v0.6.0
