# Shared test-region rules for the .claude/scripts/audit-*.sh scanners.
# Requires rust-lex.awk loaded first, and must be loaded BEFORE the consumer's
# own rules.
#
# One implementation serves both polarities:
#
#   -v skip_test_regions=1   the region rule ends in `next`, so a line inside
#                            an inline `#[cfg(test)] mod … { … }` never reaches
#                            the consumer's rules — and the line after its
#                            closing brace does, so production code that
#                            follows an inline test module is scanned like any
#                            other (audit-binary-name, audit-tag-family).
#   default                  every line reaches the consumer with the global
#                            `in_test_region` set to 1 inside such a region and
#                            0 outside, for scanners that report ONLY on test
#                            code (audit-test-isolation, audit-test-spawn-retry).
#
# The gated item is bounded the way audit-god-files.sh bounds it: from the cfg
# attribute, through any further attributes and doc comments, to the close of
# its brace block (counted on `strip_code` output so a brace inside a string
# or comment cannot end it early), or — for a braceless item such as
# `mod tests;` or a gated `use` — to its terminating `;`.
#
# `is_test_file` answers the orthogonal question the region scan cannot: a
# sibling `tests.rs` or a `crates/*/tests/**` integration file is test code in
# its entirety and carries no `#[cfg(test)]` of its own. Both alternatives
# accept a path that begins at `crates/` as well as an absolute one: a scanner
# feeds awk whatever its own `grep -rl crates/...` printed, which is relative.

function is_test_file(f) {
    return (f ~ /(^|\/)tests\.rs$/) || (f ~ /(^|\/)crates\/[^/]+\/tests\//)
}

FNR == 1 {
    reset_lex()
    test_region = 0; region_depth = 0; region_opened = 0; in_test_region = 0
}

{
    code = strip_code($0)
    in_test_region = 0

    if (test_region) {
        in_test_region = 1
        region_depth += count_char(code, "{") - count_char(code, "}")
        if (index(code, "{") > 0) region_opened = 1
        if (region_opened) {
            if (region_depth <= 0) test_region = 0
        } else if (code ~ /;[[:space:]]*$/) {
            test_region = 0
        }
    } else if (is_test_only_cfg($0)) {
        in_test_region = 1
        test_region = 1; region_depth = 0; region_opened = 0
        region_depth += count_char(code, "{") - count_char(code, "}")
        if (index(code, "{") > 0) region_opened = 1
        if (region_opened) {
            if (region_depth <= 0) test_region = 0
        } else if (code ~ /;[[:space:]]*$/) {
            test_region = 0
        }
    }

    if (in_test_region && skip_test_regions) next
}
