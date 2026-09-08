# Shared rules that hide test-only gated items from a production scanner.
# Requires rust-lex.awk loaded first, and must be loaded BEFORE the consumer's
# own rules: the region rule ends in `next`, so a line inside an inline
# `#[cfg(test)] mod … { … }` never reaches them — and the line after its
# closing brace does, so production code that follows an inline test module
# is scanned like any other.
#
# The gated item is bounded the way audit-god-files.sh bounds it: from the cfg
# attribute, through any further attributes and doc comments, to the close of
# its brace block (counted on `strip_code` output so a brace inside a string
# or comment cannot end it early), or — for a braceless item such as
# `mod tests;` or a gated `use` — to its terminating `;`.

FNR == 1 { reset_lex(); test_region = 0; region_depth = 0; region_opened = 0 }

{ code = strip_code($0) }

test_region {
    region_depth += count_char(code, "{") - count_char(code, "}")
    if (index(code, "{") > 0) region_opened = 1
    if (region_opened) {
        if (region_depth <= 0) test_region = 0
    } else if (code ~ /;[[:space:]]*$/) {
        test_region = 0
    }
    next
}

is_test_only_cfg($0) {
    test_region = 1; region_depth = 0; region_opened = 0
    region_depth += count_char(code, "{") - count_char(code, "}")
    if (index(code, "{") > 0) region_opened = 1
    if (region_opened && region_depth <= 0) test_region = 0
    next
}
