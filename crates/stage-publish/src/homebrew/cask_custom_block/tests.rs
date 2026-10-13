use super::{
    CodeLine, CustomBlockOsFacts, UnreadReason, Unreadable, code_lines, custom_block_os_facts,
};

fn facts(other_os_block: bool, os_dependency: bool) -> CustomBlockOsFacts {
    CustomBlockOsFacts {
        other_os_block,
        os_dependency,
        unread: None,
    }
}

/// The code of a line keeps everything outside its literals, writes
/// each literal that closes on the line as `""`, leaves out the part of
/// one that stays open, and stops at a `#` outside them. The code of each
/// `#{…}` is kept apart from it, one entry per interpolation.
#[test]
fn code_lines_blank_literals_and_cut_the_comment() {
    let cases = [
        (r#"url "x" # on_linux later"#, r#"url "" "#),
        ("caveats \"a # b\"", "caveats \"\""),
        ("x 'it''s'", "x \"\"\"\""),
        (r##"x "a\"#b" y"##, r#"x "" y"#),
        ("x %q(a(b)#c) y", "x \"\" y"),
        (
            "x %w[a b], %i{c}, %r<d>, %Q|e#|, %W!f!, %I/g/",
            "x \"\", \"\", \"\", \"\", \"\", \"\"",
        ),
        ("x %(a#b), %x(c), %s(d) y", "x \"\", \"\", \"\" y"),
        ("x = \"a\" %(b) # c", "x = \"\" %(b) "),
        ("a % b # c", "a % b "),
        ("n = a%(b) # c", "n = a%(b) "),
        ("x ?# y ?\" z ?' w", "x \"\" y \"\" z \"\" w"),
        ("empty?(x) # c", "empty?(x) "),
        ("a ? b : c # d", "a ? b : c "),
        ("x =~ /#/ # c", "x =~ \"\" "),
        ("y = a / b # c", "y = a / b "),
        ("f(/#/, /#/) # c", "f(\"\", \"\") "),
        ("caveats <<~EOS # c", "caveats \"\" "),
        ("m = 1<<FLAGS # c", "m = 1<<FLAGS "),
        ("m = a << 1 <<2 # c", "m = a << 1 <<2 "),
        ("@args <<extra # c", "@args <<extra "),
        ("$out <<x # c", "$out <<x "),
        ("self <<x # c", "self <<x "),
        ("end <<~EOS # c", "end <<~EOS "),
        ("nil ?\"a\" : 1 # c", "nil ?\"\" : 1 "),
        ("y = 1; x = y <<z # c", "y = 1; x = y <<z "),
        ("each { |a, b| a <<b } # c", "each { |a, b| a <<b } "),
        ("each do |x| x %-2 end # c", "each do |x| x %-2 end "),
        ("obj.y = 1; y <<~EOS", "obj.y = 1; y \"\""),
        ("z == 1; z <<~EOS", "z == 1; z \"\""),
    ];
    for (line, want) in cases {
        assert_eq!(
            code_lines(line).0,
            [CodeLine {
                text: want.to_string(),
                open: false,
                interpolations: Vec::new(),
            }],
            "{line}"
        );
    }
    let cases: &[(&str, &str, &[&str])] = &[
        ("\"a#{\"}\"}b\" # c", "\"\" ", &["\"\""]),
        ("\"a#{'\"'}b\" # c", "\"\" ", &["\"\""]),
        ("\"#{{a: 1}[:a]}\" x", "\"\" x", &["{a: 1}[:a]"]),
        ("x \"#{a}b#{c}\"", "x \"\"", &["a", "c"]),
        ("x \"#{\"#{a}\" b}\"", "x \"\"", &["a", "\"\" b"]),
        ("x \"#{a; $;}\"; y", "x \"\"; y", &["a; $_"]),
    ];
    for (line, text, interpolations) in cases {
        assert_eq!(
            code_lines(line).0,
            [CodeLine {
                text: text.to_string(),
                open: false,
                interpolations: interpolations.iter().map(|c| c.to_string()).collect(),
            }],
            "{line}"
        );
    }
}

/// A literal or heredoc that does not close on its line carries on into
/// the following lines; its content is left out until it closes, and the
/// code of a `#{…}` in it is one entry on the line the `#{…}` closes on.
#[test]
fn an_open_literal_carries_into_the_next_lines() {
    let line = |text: &str, open: bool| CodeLine {
        text: text.to_string(),
        open,
        interpolations: Vec::new(),
    };
    let closing = |text: &str, code: &str| CodeLine {
        interpolations: vec![code.to_string()],
        ..line(text, false)
    };
    let cases: &[(&str, &[CodeLine])] = &[
        (
            "x \"open # not a comment\non_linux\" y",
            &[line("x ", true), line("\"\" y", false)],
        ),
        (
            "puts %w[\n on_linux\n] y",
            &[line("puts ", true), line("", true), line("\"\" y", false)],
        ),
        (
            "\"#{\n x \"}\" \n}\" y",
            &[
                line("", true),
                line("", true),
                closing("\"\" y", "\n x \"\" \n"),
            ],
        ),
        (
            "caveats <<~EOS, <<-'B'\n on_linux\n  EOS\n  B\nz",
            &[
                line("caveats \"\", \"\"", false),
                line("", false),
                line("", false),
                line("", false),
                line("z", false),
            ],
        ),
        (
            "puts <<EOS\n  EOS\neos\nEOS\nz",
            &[
                line("puts \"\"", false),
                line("", false),
                line("", false),
                line("", false),
                line("z", false),
            ],
        ),
    ];
    for (block, want) in cases {
        assert_eq!(code_lines(block), (want.to_vec(), None), "{block}");
    }
}

/// What `custom_block_os_facts` reads from a `custom_block`, case by
/// case. Both facts read the code of each line. A Linux side holds when
/// its word appears there as a whole word. An OS dependency holds when
/// a statement that opens with `depends_on`, read across its
/// continuation lines, names an OS key or symbol. A wrong verdict
/// renders a cask Homebrew refuses, so the over-match below is the case
/// `depends_on_macos: true` exists for and the misses are the cases for
/// `depends_on_macos: false`.
#[test]
fn custom_block_os_facts_reads_whole_words_in_the_code() {
    let cases: &[(&str, &str, CustomBlockOsFacts)] = &[
        ("empty", "", facts(false, false)),
        (
            "top-level on_linux",
            "on_linux do\n  binary \"x\"\nend",
            facts(true, false),
        ),
        (
            "top-level on_system",
            "on_system :linux do\nend",
            facts(true, false),
        ),
        ("app_image", "app_image \"X.AppImage\"", facts(true, false)),
        (
            "on_linux nested in on_macos",
            "on_macos do\n  on_linux do\n  end\nend",
            facts(true, false),
        ),
        (
            "depends_on macos: nested in on_macos",
            "on_macos do\n  depends_on macos: \">= :sonoma\"\nend",
            facts(false, true),
        ),
        (
            "depends_on after ; in a one-line block",
            "on_macos do; depends_on macos: :sonoma; end",
            facts(false, true),
        ),
        (
            "depends_on spread over several lines",
            "depends_on(\n  macos: \">= :big_sur\"\n)",
            facts(false, true),
        ),
        (
            "depends_on macos: with a symbol in its value",
            "depends_on macos: \">= :big_sur\"",
            facts(false, true),
        ),
        (
            "an OS name in a comment after a depends_on without one",
            "depends_on formula: \"jq\" # needs :macos 13",
            facts(false, false),
        ),
        (
            "an OS name in a comment on the next line",
            "depends_on formula: \"jq\"\n# needs :macos 13",
            facts(false, false),
        ),
        (
            "an OS name in the statement after the depends_on",
            "depends_on formula: \"jq\"; caveats \":macos\"",
            facts(false, false),
        ),
        (
            "a whole-line comment naming an OS dependency",
            "  # depends_on macos: \">= :ventura\"",
            facts(false, false),
        ),
        (
            "a whole-line comment naming a Linux side",
            "# on_linux do\n#   binary \"x\"\n# end",
            facts(false, false),
        ),
        (
            "on_linux in a comment after code",
            "url \"x\" # on_linux later",
            facts(false, false),
        ),
        (
            "a # inside a string cuts nothing",
            "caveats \"a # b\"\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a # inside a %q literal cuts nothing",
            "depends_on formula: %q(a#b), macos: :ventura",
            facts(false, true),
        ),
        (
            "a # inside a %q literal with a symmetric delimiter",
            "depends_on formula: %q|a#b|, macos: :ventura",
            facts(false, true),
        ),
        (
            "depends_on inside a string",
            "caveats \"this depends_on macos: newer\"",
            facts(false, false),
        ),
        (
            "a ; inside a string starts no statement",
            "caveats \"a; depends_on macos: :ventura\"",
            facts(false, false),
        ),
        (
            "a ) inside a string does not close the call",
            "depends_on(formula: \"a)b\",\n  macos: \">= :ventura\")",
            facts(false, true),
        ),
        (
            "a trailing comma continues the statement",
            "depends_on arch: :arm64,\n  macos: \">= :ventura\"",
            facts(false, true),
        ),
        (
            "a comment line inside a continued statement",
            "depends_on arch: :arm64,\n  # newer systems only\n  macos: :ventura",
            facts(false, true),
        ),
        (
            "a blank line inside a continued statement",
            "depends_on arch: :arm64,\n\n  macos: :ventura",
            facts(false, true),
        ),
        (
            "a trailing backslash continues the statement",
            "depends_on arch: :arm64 \\\n  , macos: :ventura",
            facts(false, true),
        ),
        (
            "an open [ continues the statement until it closes",
            "depends_on cask: [\n  \"a\",\n], macos: :ventura",
            facts(false, true),
        ),
        (
            "an open { continues the statement until it closes",
            "depends_on({\n  formula: \"a\"\n}.merge(macos: :ventura))",
            facts(false, true),
        ),
        (
            "a closed list ends the statement",
            "depends_on cask: [\"a\"]\nmacos: :ventura",
            facts(false, false),
        ),
        (
            "a trailing comment is cut before the comma is read",
            "depends_on arch: :arm64, # :macos below\n  formula: \"x\"",
            facts(false, false),
        ),
        (
            "depends_on as the last token of a line",
            "x = depends_on\nmacos: :ventura",
            facts(false, false),
        ),
        (
            "a bare depends_on ends its statement",
            "depends_on\nmacos: :ventura",
            facts(false, false),
        ),
        (
            "a second ;-statement opens with depends_on",
            "depends_on formula: \"x\"; depends_on macos: :ventura",
            facts(false, true),
        ),
        (
            "a depends_on line inside an open { starts its own statement",
            "foo = {\n  depends_on macos: :ventura\n}",
            facts(false, true),
        ),
        (
            "an OS key on a later line of a depends_on( call",
            "depends_on(\n  formula: \"jq\",\n  linux: :any\n)",
            facts(false, true),
        ),
        (
            "a constant path is not a symbol",
            "depends_on formula: Foo::macos",
            facts(false, false),
        ),
        (
            "maximum_macos: contains macos:",
            "depends_on maximum_macos: :sonoma",
            facts(false, true),
        ),
        ("bare :macos", "depends_on :macos", facts(false, true)),
        ("linux: hash", "depends_on linux: :any", facts(false, true)),
        (
            "a depends_on without an OS",
            "depends_on formula: \"x\"",
            facts(false, false),
        ),
        (
            "an OS name without a depends_on",
            "caveats \":macos\"",
            facts(false, false),
        ),
        (
            "an OS name inside a string of a depends_on",
            "depends_on cask: \":macos\"",
            facts(false, false),
        ),
        (
            "on_linux in a string and in a comment",
            "caveats \"on_linux\"\n# on_linux",
            facts(false, false),
        ),
        (
            "an apostrophe in a %q string, then on_linux",
            "caveats %q(Don't forget)\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << shift, then on_linux",
            "m = 1<<FLAGS\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << after a call is a shift, then on_linux",
            "x = foo() <<x\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << after an index is a shift, then on_linux",
            "x = list[0] <<y\non_linux do\nend",
            facts(true, false),
        ),
        (
            "class <<self is a shift, then on_linux",
            "class <<self\nend\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a ? after a call is a ternary, then on_linux",
            "x = foo() ?\"a\" : \"b\"\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a ? before a quote and a word is a ternary, then on_linux",
            "x = c ?\"a\" : \"b\"\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a % after a parenthesis is a modulo, then on_linux",
            "n = (a) %-2\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << after a local is a shift, then on_linux",
            "y = 1\nx = y <<z\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << after an instance variable is a shift, then on_linux",
            "@args <<extra\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << after a global variable is a shift, then on_linux",
            "$stdout <<extra\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << after self is a shift, then on_linux",
            "self <<x\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << after end is a shift, then on_linux",
            "end <<~EOS\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a % after a local is a modulo, then on_linux",
            "m = 7\nn = m %-2\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a ? after a local before an empty string is a ternary, then on_linux",
            "cond = true\nx = cond ?\"\" : \"no\"\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a ? after a local before a quoted space is a ternary, then on_linux",
            "cond = true\nx = cond ?' ' : ''\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a ? after a block parameter is a ternary, then on_linux",
            "[1].each do |cond|\n  x = cond ?\"\" : \"no\"\nend\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << after a brace block parameter is a shift, then on_linux",
            "list.map { |a, b| a <<b }\non_linux do\nend",
            facts(true, false),
        ),
        (
            "a << after nil, true or false is a shift, then on_linux",
            "nil <<a; true <<b; false <<c\non_linux do\nend",
            facts(true, false),
        ),
        (
            "an == comparison assigns no local, so its percent literal hides on_linux",
            "z == 1\nz %(on_linux)",
            facts(false, false),
        ),
        (
            "an attribute assignment assigns no local, so its percent literal hides on_linux",
            "obj.y = 1\ny %(on_linux)",
            facts(false, false),
        ),
        (
            "a heredoc after a method name hides on_linux",
            "caveats <<~EOS\n  on_linux\nEOS\n",
            facts(false, false),
        ),
        (
            "two heredocs as arguments hide on_linux",
            "puts(<<~A, <<~B)\n on_linux\nA\n on_linux\nB\n",
            facts(false, false),
        ),
        (
            "a quoted heredoc tag with a dash, then on_linux",
            "caveats <<~'END-TEXT'\n  text\nEND-TEXT\non_linux do\nend",
            facts(true, false),
        ),
        (
            "on_linux after a string with a ; inside it",
            "caveats \"a; on_linux\"; on_linux do\nend",
            facts(true, false),
        ),
        (
            "on_linux behind a receiver",
            "self.on_linux do\nend",
            facts(true, false),
        ),
        (
            "on_linux inside parentheses",
            "(on_linux do\nend)",
            facts(true, false),
        ),
        (
            "on_linux nested on one line",
            "on_arm do on_linux do binary \"x\" end end",
            facts(true, false),
        ),
        (
            "on_linux inside a %w[] list over several lines",
            "%w[\n on_linux\n]",
            facts(false, false),
        ),
        (
            "on_linux on a heredoc body line",
            "caveats <<~EOS\n  on_linux\nEOS\n",
            facts(false, false),
        ),
        (
            "a heredoc body, then a depends_on",
            "caveats <<~EOS\non_linux\nEOS\ndepends_on macos: :big_sur",
            facts(false, true),
        ),
        (
            "a string over two lines, then a depends_on",
            "caveats \"a\nb\"; depends_on macos: :ventura",
            facts(false, true),
        ),
        (
            "a string over two lines, then on_linux",
            "caveats \"a\nb\"; on_linux do\nend",
            facts(true, false),
        ),
        (
            "on_linux in a string inside an interpolation",
            "\"#{\"on_linux\"}\"",
            facts(false, false),
        ),
        (
            "a quote inside an interpolation does not close the string",
            "\"a#{'\"'}b\"; on_linux do\nend",
            facts(true, false),
        ),
        (
            "on_linux in a bare %() literal",
            "caveats %(on_linux)",
            facts(false, false),
        ),
        (
            "a # inside a bare %() literal cuts nothing",
            "depends_on formula: %(a#b), macos: :ventura",
            facts(false, true),
        ),
        (
            "a # in a regex after =~ cuts nothing",
            "x =~ /#/; on_linux do\nend",
            facts(true, false),
        ),
        (
            "a # in a char literal cuts nothing",
            "x = ?#; on_linux do\nend",
            facts(true, false),
        ),
        (
            "a trailing => continues the statement",
            "depends_on :formula =>\n \"x\", :macos => :ventura",
            facts(false, true),
        ),
        (
            "a depends_on statement inside parentheses",
            "(depends_on macos: :ventura)",
            facts(false, true),
        ),
        (
            "a string spanning lines inside a depends_on",
            "depends_on formula: \"a\nb\", macos: :ventura",
            facts(false, true),
        ),
        (
            "a string spanning lines holding an OS key",
            "depends_on formula: \"a\nmacos: b\"",
            facts(false, false),
        ),
        (
            "a longer word holding on_linux or depends_on",
            "end_foo\non_linux_helper\nmy_on_system\ndepends_on_x :macos\n",
            facts(false, false),
        ),
        // A regex after a keyword or a method name is set aside, and a
        // `/` after a value divides.
        (
            "on_linux in a regex after when",
            "case x\nwhen /on_linux/ then 1\nend",
            facts(false, false),
        ),
        (
            "on_linux in a regex after a method name",
            "y = 1\nok = x.y /on_linux/",
            facts(false, false),
        ),
        (
            "on_linux divided after a local",
            "x = 4\ny = x /on_linux/ 2",
            facts(true, false),
        ),
        // Misses: a Linux side built at runtime, which `depends_on_macos:
        // false` exists for.
        (
            "an on_linux built at runtime",
            "send(:\"on_#{\"lin\"}ux\") do\n  url \"https://e.com/l.tar.gz\"\nend\n",
            facts(false, false),
        ),
        (
            "a depends_on after a do on the same line",
            "on_macos do depends_on macos: :ventura end",
            facts(false, false),
        ),
        (
            "an on_linux defined at runtime",
            "define_method(:x) { send(\"on_\" + \"linux\") { } }",
            facts(false, false),
        ),
    ];
    for (label, block, want) in cases {
        assert_eq!(
            custom_block_os_facts(Some(block)),
            *want,
            "{label}:\n{block}"
        );
    }
    assert_eq!(custom_block_os_facts(None), facts(false, false));
}

fn unclosed(line: usize, kind: &'static str) -> Option<Unreadable> {
    Some(Unreadable {
        line,
        kind,
        reason: UnreadReason::Unclosed,
    })
}

fn ambiguous(line: usize, kind: &'static str, operator: &'static str) -> Option<Unreadable> {
    Some(Unreadable {
        line,
        kind,
        reason: UnreadReason::Ambiguous { operator },
    })
}

/// The facts for `block`, asserted against the verdict and the place,
/// if any, the block could not be read past.
fn assert_facts(
    block: &str,
    other_os_block: bool,
    os_dependency: bool,
    unread: Option<Unreadable>,
) {
    assert_eq!(
        custom_block_os_facts(Some(block)),
        CustomBlockOsFacts {
            other_os_block,
            os_dependency,
            unread,
        },
        "{block}"
    );
}

/// A literal or heredoc still open after the last line means the block
/// was misread. Both facts then come from a word scan of the raw text
/// with its comments cut, and `unread` names the line the open literal
/// starts on and its kind.
#[test]
fn an_unclosed_literal_falls_back_to_the_word_scan() {
    let cases: &[(&str, &str, CustomBlockOsFacts)] = &[
        (
            "an unclosed string hides nothing after it",
            "url \"x\"\ncaveats \"never closed\non_linux do\nend",
            CustomBlockOsFacts {
                other_os_block: true,
                os_dependency: false,
                unread: unclosed(2, "string"),
            },
        ),
        (
            "a ? after a name that is no local reads a character literal, and the string after it never closes",
            "x = cond ?\"\" : \"no\"\ndepends_on macos: :ventura",
            CustomBlockOsFacts {
                other_os_block: false,
                os_dependency: true,
                unread: ambiguous(1, "character literal", "?"),
            },
        ),
        (
            "a heredoc with no closing tag",
            "caveats <<~EOS\n  text\non_linux do\nend",
            CustomBlockOsFacts {
                other_os_block: true,
                os_dependency: false,
                unread: unclosed(1, "heredoc"),
            },
        ),
        (
            "an unclosed percent literal; the word scan reads comments too",
            "x = %w[a\n# on_linux\n",
            CustomBlockOsFacts {
                other_os_block: true,
                os_dependency: false,
                unread: unclosed(1, "percent literal"),
            },
        ),
        (
            "an unclosed regex; an OS word with no depends_on is no dependency",
            "x =~ /a\n:macos",
            CustomBlockOsFacts {
                other_os_block: false,
                os_dependency: false,
                unread: unclosed(1, "regex"),
            },
        ),
    ];
    for (label, block, want) in cases {
        assert_eq!(
            custom_block_os_facts(Some(block)),
            *want,
            "{label}:\n{block}"
        );
    }
}

/// The word scan reads the raw text, comments and strings included, so a
/// `#` inside a string hides nothing after it.
#[test]
fn the_word_scan_reads_past_a_hash_in_a_string() {
    assert_facts(
        "x = width %-2\ndesc \"the # key\"; on_linux do\nend\nn = 3 - 1",
        true,
        false,
        ambiguous(1, "percent literal", "%"),
    );
    assert_facts(
        "caveats <<~EOS\ndepends_on formula: \"a\" # macos: 13\n",
        false,
        true,
        unclosed(1, "heredoc"),
    );
}

/// An untracked word before a `<<`, `%` or `?` may be a local the lexer
/// did not see: the operator is read as an opener, and when the literal
/// it opens spans more than one line the block falls back to the word
/// scan. One that closes on its line stays read as code.
#[test]
fn an_ambiguous_opener_spanning_lines_falls_back_to_the_word_scan() {
    assert_facts(
        "x = width %-2\non_linux do\nend\nn = 3 - 1",
        true,
        false,
        ambiguous(1, "percent literal", "%"),
    );
    assert_facts("y = width %(a) + 1\non_linux do\nend", true, false, None);
    assert_facts(
        "z <<~EOS\n  on_linux\nEOS\n",
        true,
        false,
        ambiguous(1, "heredoc", "<<"),
    );
    assert_facts("caveats <<~EOS\n  on_linux\nEOS\n", false, false, None);
}

/// `a, b = 10, 3` binds both names, so `a %-b` is a modulo.
#[test]
fn a_multiple_assignment_binds_every_name() {
    assert_facts(
        "a, b = 10, 3\nr = a %-b\ndepends_on macos: \">= :ventura\"\npad = 1 - 2",
        false,
        true,
        None,
    );
}

/// `flag ||= true` binds `flag`, so `flag ?'-'` is a ternary.
#[test]
fn an_operator_assignment_binds_its_name() {
    assert_facts(
        "flag ||= true\nsep = flag ?'-' : '_'\non_linux do\n  binary \"tool\"\nend\n# it's built per arch",
        true,
        false,
        None,
    );
    for operator in ["&&=", "+=", "-=", "*=", "/="] {
        assert_facts(
            &format!("n {operator} 1\nm = n %-2\npad = 3 - 1\non_linux do\nend"),
            true,
            false,
            None,
        );
    }
}

/// `for n in [1, 2]` binds `n`, so `n %-2` is a modulo.
#[test]
fn a_for_loop_binds_its_variable() {
    assert_facts(
        "for n in [1, 2]\n  m = n %-2\nend\ndepends_on macos: :ventura\npad = 3 - 1",
        false,
        true,
        None,
    );
}

/// Every parameter of a `def`, with or without parentheses, defaults,
/// `*rest`, `**kw` and `&blk`, is read as a value.
#[test]
fn a_method_definition_binds_its_parameters() {
    assert_facts(
        "def self.wrap(a, b = 1)\n  a %-b\nend\non_linux do\n  binary \"tool\"\nend\npad = 3 - 1",
        true,
        false,
        None,
    );
    for name in ["a", "b", "rest", "kw", "blk", "k"] {
        for head in [
            "def wrap(a, b = [1, 2], *rest, k: 1, **kw, &blk)",
            "def wrap a, b = 1, *rest, k: 1, **kw, &blk",
        ] {
            assert_facts(
                &format!("{head}\n  {name} %-2\nend\npad = 3 - 1\non_linux do\nend"),
                true,
                false,
                None,
            );
        }
    }
}

/// `->(x)` and `-> (x)` bind `x` for the lambda body on the same line.
#[test]
fn a_lambda_binds_its_parameters() {
    for head in ["->(x)", "-> (x)"] {
        assert_facts(
            &format!("half = {head} {{ x %-2 }}\ndepends_on macos: :ventura\npad = 3 - 1"),
            false,
            true,
            None,
        );
    }
}

/// `rescue => e` binds `e`, so `e %-1` is a modulo.
#[test]
fn a_rescue_binds_its_variable() {
    assert_facts(
        "begin\n  Integer(\"x\")\nrescue => e\n  e %-1 rescue nil\nend\non_system :linux, macos: :ventura do\n  binary \"tool\"\nend\npad = 3 - 1",
        true,
        false,
        None,
    );
}

/// `(lo, hi) = 1, 9` binds both names, so `lo %-hi` is a modulo.
#[test]
fn a_parenthesized_multiple_assignment_binds_every_name() {
    assert_facts(
        "(lo, hi) = 1, 9\nr = lo %-hi\ndepends_on macos: :ventura\npad = 3 - 1",
        false,
        true,
        None,
    );
}

/// A block parameter list spread over several lines binds every name.
#[test]
fn a_block_parameter_list_over_several_lines_binds_every_name() {
    assert_facts(
        "v = [[1, 2]].map do |a,\n  b|\n  a %-b\nend\non_linux do\n  binary \"tool\"\nend\npad = 3 - 1",
        true,
        false,
        None,
    );
}

/// A block parameter is a local only inside its block. There, a `<<`
/// after its name whose heredoc tag closes on a later line is ambiguous, and
/// the block falls back to the word scan with a warning; a `<<` whose tag
/// never closes can only be a shift. After the block the name is untracked,
/// so `caveats <<~EOS` opens a heredoc.
#[test]
fn a_heredoc_after_a_block_parameter_falls_back_to_the_word_scan() {
    assert_facts(
        "[1].each do |caveats|\n  caveats <<~EOS\n  Runs on_linux too.\n  EOS\nend",
        true,
        false,
        ambiguous(2, "heredoc", "<<"),
    );
    assert_facts(
        "[1].each do |caveats| end\ncaveats <<~EOS\n  Runs on_linux too.\nEOS",
        false,
        false,
        None,
    );
    assert_facts(
        "list.map { |a, b| a <<b }\non_linux do\nend",
        true,
        false,
        None,
    );
}

/// Each line ending continues a `depends_on` statement onto the next
/// line, so an OS key there is read as part of it.
#[test]
fn every_continuation_ending_joins_the_next_line() {
    for ending in [",", "(", "[", "{", "|", ".", "&&", "||", "=>", "\\"] {
        let block = format!("depends_on formula: \"x\" {ending}\n  macos: :ventura");
        assert_eq!(
            custom_block_os_facts(Some(&block)),
            facts(false, true),
            "{ending}"
        );
    }
}

/// A `/` after `when` or after a method name opens a regex, so a quote or
/// a `#` inside it opens nothing. After a local it divides. After a word
/// the lexer cannot place, a regex that spans lines is a guess.
#[test]
fn a_slash_opens_a_regex_where_an_operand_stands() {
    assert_facts(
        "case x\nwhen /'/ then 1\nend\non_linux do\nend",
        true,
        false,
        None,
    );
    assert_facts("ok = x.match? /#/\non_linux do\nend", true, false, None);
    assert_facts(
        "a = 8\nb = 2\nc = 1\nd = a / b / c\non_linux do\nend",
        true,
        false,
        None,
    );
    assert_facts(
        "x = width /2\non_linux do\nend\n/",
        true,
        false,
        ambiguous(1, "regex", "/"),
    );
}

/// `a, (b, c) = …` binds the names inside the parentheses too.
#[test]
fn a_nested_multiple_assignment_binds_every_name() {
    assert_facts(
        "a, (b, c) = 1, [2, 3]\nn = c %-2\non_linux do\nend\npad = 3 - 1",
        true,
        false,
        None,
    );
}

/// A setter `def size=(v)` binds `v` inside its body and leaves `size`
/// unbound after it.
#[test]
fn a_setter_definition_binds_only_its_parameter() {
    assert_facts(
        "def size=(v)\n  @v = v %-2\nend\non_linux do\nend\npad = 3 - 1",
        true,
        false,
        None,
    );
    assert_facts(
        "def size=(v)\nend\nn = size %-2\non_linux do\nend\n-",
        true,
        false,
        ambiguous(3, "percent literal", "%"),
    );
}

/// A `def` body sees none of the locals outside it, a block sees them
/// all, and a block parameter is gone after the block's `end`.
#[test]
fn a_method_body_hides_outer_locals_and_a_block_keeps_them() {
    assert_facts(
        "n = 4\ndef f\n  n %-2\nend\non_linux do\nend\n-\nend",
        true,
        false,
        ambiguous(3, "percent literal", "%"),
    );
    assert_facts(
        "[1].each do |m|\nend\nm %-2\non_linux do\nend\n-",
        true,
        false,
        ambiguous(3, "percent literal", "%"),
    );
    assert_facts(
        "n = 4\n[1].each do\n  r = n %-2\nend\non_linux do\nend\npad = 3 - 1",
        true,
        false,
        None,
    );
}

/// An `end` or a `}` with nothing open, a `(` that never closes before an
/// `=`, and an assignment target nested thousands of groups deep are read
/// without a panic or a stack overflow.
#[test]
fn an_unmatched_end_is_read_without_a_panic() {
    let deep = format!("{}a{} = 1", "(".repeat(100_000), ")".repeat(100_000));
    for block in [
        "end\nend\n}\non_linux do\nend",
        "}\n}\nend\nx = 1\ndef f\nend\nend\nend",
        "(a = 1",
        &deep,
    ] {
        let _ = custom_block_os_facts(Some(block));
    }
    assert_facts("end\nend\n}\non_linux do\nend", true, false, None);
}

/// A named capture before `=~`, a name in an `in` pattern and the target
/// of a rightward `=>` are bound.
#[test]
fn captures_patterns_and_rightward_assignments_bind_their_names() {
    for head in [
        "/(?<w>\\d+)/ =~ s",
        "case [1, 2]\nin [w, 2]\nend",
        "2 => w",
        "{a: 1} in {a: w}",
    ] {
        assert_facts(
            &format!("{head}\nr = w %-2\non_linux do\nend\npad = 3 - 1"),
            true,
            false,
            None,
        );
    }
}

/// Literals Ruby opens or declines where few blocks write them. Each
/// verdict matches what Ruby's own parser reads.
#[test]
fn rare_literal_forms_are_read_the_way_ruby_reads_them() {
    let cases: &[(&str, bool)] = &[
        // `$"` is a global, so its `"` opens no string.
        ("x = $\"\non_linux do\nend", true),
        // A `%` at the end of a line takes the next line as its text.
        ("x = [%\non_linux do\n]", false),
        // A `\` delimiter closes the literal it opened.
        ("x = %\\a\\\non_linux do\nend", true),
        ("x = `uname #{y} on_linux`", false),
        // `` :` `` is a symbol.
        ("x = :`\non_linux do\nend", true),
        // A keyword takes an operand with no space before it.
        ("x = 1 if%w[\non_linux\n]", false),
        ("2if%w[\non_linux\n]", false),
        // A typed literal takes a space as its delimiter.
        ("puts %x on_linux \n", false),
        ("h = {p:%(on_linux do)}", false),
        // After a number the `%` is a modulo.
        ("x = 0x1f %w[\non_linux\n]", true),
        // `?#` is a character literal, so `#` starts no comment.
        ("x = ?#if %w[\non_linux\n]", false),
        // `then?` is a method name and its `"` opens a string.
        ("x.then?\"%(\non_linux do\nend\n\"", false),
        // `.<<` and `::<<` name a method, so `<<"EOS"` opens no heredoc.
        ("x = y.<<\"EOS\"\non_linux do\nend\nEOS", true),
        ("x = y::<<\"EOS\"\non_linux do\nend\nEOS", true),
        ("def /(other)\n  other\nend\non_linux do\nend", true),
        // `#$"` interpolates `$"`, so its `"` does not close the string.
        ("x = \"a#$\"\non_linux do\nend\"", false),
    ];
    for (block, other_os_block) in cases {
        assert_facts(block, *other_os_block, false, None);
    }
}

/// A heredoc after any cask stanza is read as one: every stanza takes an
/// argument, so none of them is a guess. `app_image` is a Linux side.
#[test]
fn a_heredoc_after_every_cask_stanza_is_read_as_one() {
    for word in super::words::CASK_DSL_WORDS {
        assert_facts(
            &format!("{word} <<~EOS\n x\nEOS"),
            *word == "app_image",
            false,
            None,
        );
    }
}

/// The cask DSL list holds the 82 distinct names of Homebrew 7.0.8's
/// `DSL_METHODS`: 42 written out, 32 ordinary artifacts, 4 from the 2 block
/// artifacts and 4 install steps.
#[test]
fn the_cask_dsl_list_holds_every_dsl_method_once() {
    let words = super::words::CASK_DSL_WORDS;
    let distinct: std::collections::BTreeSet<_> = words.iter().collect();
    assert_eq!(distinct.len(), words.len(), "a name is listed twice");
    assert_eq!(words.len(), 42 + 32 + 2 * 2 + 4);
}

/// A line that ends in `\` continues its statement onto the next line, and
/// so does a `?\` character literal, which holds the newline: the code
/// before the break still decides what the next line's `%`, `if` or
/// `until` is.
#[test]
fn a_statement_continued_with_a_backslash_reads_on_into_the_next_line() {
    assert_facts(
        "n = 7\nm = n \\\n  % 2\non_linux do\nend",
        true,
        false,
        None,
    );
    assert_facts(
        "n = 7\nm = n \\\n  % 2\ndepends_on macos: \">= :ventura\"",
        false,
        true,
        None,
    );
    assert_facts("x = ?\\\n% on_linux do\nend", true, false, None);
    assert_facts(
        "w = 1\ndef m\n  puts \"a\" \\\n    if true\nend\nr = w %(on_linux)",
        true,
        false,
        None,
    );
    assert_facts(
        "w = 1\ndef m\n  x = 1 \\\n    until true\nend\nr = w %(on_linux)",
        true,
        false,
        None,
    );
}

/// `-> { … }` is the body of a lambda, so a name first assigned inside it
/// is gone after its `}`.
#[test]
fn a_lambda_brace_body_does_not_leak_its_names() {
    assert_facts("f = -> { w = 1 }\nr = w %w[on_linux]", false, false, None);
    assert_facts("f = ->{ w = 1 }\nr = w %w[on_linux]", false, false, None);
    assert_facts("w = 1\nf = -> { w }\nr = w %(on_linux)", true, false, None);
}

/// A `\` at the end of a body line of a heredoc whose tag is not
/// single-quoted joins the next line to it, so a tag right after it is body
/// text. A single-quoted tag joins nothing, and an even run of `\` is an
/// escaped backslash that joins nothing either.
#[test]
fn a_backslash_ending_a_heredoc_body_line_joins_the_next_line() {
    for opener in ["<<~EOS", "<<-EOS", "<<EOS", "<<~\"EOS\"", "<<~`EOS`"] {
        assert_facts(
            &format!("x = {opener}\n  a \\\nEOS\n  on_linux\nEOS"),
            false,
            false,
            None,
        );
    }
    assert_facts("x = <<~'EOS'\n  a \\\nEOS\non_linux", true, false, None);
    assert_facts("x = <<~EOS\n  a \\\\\nEOS\non_linux", true, false, None);
}

/// A typed percent literal whose delimiter is the end of its line holds the
/// next line, as a bare `%` does, and that line ending in an odd run of `\`
/// joins the line after it. The newline that closes it ends no statement,
/// so a `%` or `if` on the next line follows a value. A list (`%w`, `%i`)
/// splits its words on whitespace, so a space or the end of the line never
/// closes it.
#[test]
fn a_percent_literal_delimited_by_the_end_of_its_line_holds_the_next() {
    for kind in ["", "q", "Q", "s", "r", "x"] {
        assert_facts(&format!("x = %{kind}\n  on_linux \n"), false, false, None);
        // After a method name only a typed literal opens; a bare `%` is a
        // modulo.
        assert_facts(
            &format!("p %{kind}\n  on_linux \n"),
            kind.is_empty(),
            false,
            None,
        );
        assert_facts(
            &format!("v = %{kind}\nab \\\non_linux\n"),
            false,
            false,
            None,
        );
        assert_facts(
            &format!("n = %{kind}\nab\n% 2\non_linux do\nend"),
            true,
            false,
            None,
        );
    }
    for kind in ["w", "W", "i", "I"] {
        for block in [
            format!("x = %{kind}\n  on_linux \n"),
            format!("x = %{kind} a b \non_linux"),
        ] {
            assert_facts(&block, true, false, unclosed(1, "percent literal"));
        }
    }
    let edges = super::line_code_edges("x = %q\n  on_linux \n.strip");
    assert_eq!(
        edges.iter().map(|e| e.starts_in_code).collect::<Vec<_>>(),
        [true, false, true]
    );
}

/// In the body of a heredoc whose tag is not single-quoted, `#{…}` is code:
/// a heredoc opened there has its body read from the next line, before the
/// rest of the body around it, and a `#{…}` left open at the end of a body
/// line reads on into the next. A single-quoted tag reads `#{` as text.
#[test]
fn an_interpolation_in_a_heredoc_body_is_read_as_code() {
    let cases: &[(&str, bool, Option<Unreadable>)] = &[
        ("x = <<~EOS\n  #{<<~X}\nEOS\nX\non_linux\nEOS", false, None),
        (
            "x = <<~EOS\n  #{<<~X}\nEOS\nX\nEOS\non_linux do\nend",
            true,
            None,
        ),
        (
            "x = <<~EOS\n #{<<~A} #{<<~B}\nA\nB\non_linux\nEOS",
            false,
            None,
        ),
        ("x = <<~EOS\n  #{\"a\"}\n  on_linux\nEOS", false, None),
        ("x = <<~EOS\n  #{\"}\"}\nEOS\non_linux do\nend", true, None),
        (
            "x = <<~'EOS'\n  #{<<~X}\nEOS\nX\non_linux do\nend",
            true,
            None,
        ),
        (
            "x = <<~EOS\n  #{a \\\n }\nEOS\non_linux do\nend",
            true,
            None,
        ),
    ];
    for (block, other_os_block, unread) in cases {
        assert_facts(block, *other_os_block, false, *unread);
    }
}

/// After `.`, `&.` or `::`, with or without a space, the next token is a
/// method name whatever it looks like, so a `%`, `!~` or `<<` there opens
/// nothing, and a `%` written straight after such a name is a modulo.
#[test]
fn an_operator_after_a_call_operator_is_a_method_name() {
    for block in [
        "fself. % on_linux do\nend",
        "nil\n&.!~% on_linux do\nend",
        "x:: % on_linux do\nend",
        "x.  <<  on_linux do\nend",
        "x = 1.<=>?#\non_linux do\nend",
    ] {
        assert_facts(block, true, false, None);
    }
}

/// A line ending in `.`, `&.` or `::` names its method on a later line,
/// past blank and comment lines, so the statement reads on and the `%` or
/// `<<` that starts the next line is the method name. A `..` range at the
/// end of a line reads nothing on.
#[test]
fn a_call_operator_at_the_end_of_a_line_continues_the_statement() {
    for block in [
        "y = x.\n%(on_linux)",
        "foo = 1\nfoo&.\n%(on_linux)",
        "A::\n%(on_linux)",
        "y = x.\n  # c\n\n%(on_linux)",
        "y = x.\n<<~EOS\non_linux\nEOS",
    ] {
        assert_facts(block, true, false, None);
    }
    assert_facts("x = 1..\n%(on_linux)", false, false, None);
}

/// A `%r` closed by a newline takes the letters that start the next line
/// as its options, and the statement reads on after them, so the line
/// begins inside the literal and its trim keeps the options.
#[test]
fn a_regex_closed_by_a_newline_takes_the_next_line_s_letters_as_options() {
    assert_facts("x = %r\nab\nix + 1", false, false, None);
    for block in [
        "x = %r\nab\ni % 2\non_linux do\nend",
        "x = %r\nab\nix.size\non_linux do\nend",
        "x = %r\nab\n\non_linux do\nend",
    ] {
        assert_facts(block, true, false, None);
    }
    let edges = super::line_code_edges("x = %r\nab\nix  \ny = 1");
    assert_eq!(
        edges[2],
        super::LineEdges {
            starts_in_code: false,
            trim_from: Some(2),
        }
    );
    assert!(edges[3].starts_in_code);
}

/// A `::` that ends a line, or stands before a space, makes the word after
/// it a constant path, as one written on the same line does, so the `%`
/// after that word opens a literal.
#[test]
fn a_word_after_a_double_colon_on_an_earlier_line_is_a_constant_path() {
    for block in [
        "y = A::\nB %(on_linux)",
        "x = 1\ny = A::\nx %(on_linux)",
        "x = A::\n  B %(on_linux)",
        "x = A::\n# c\nB %(on_linux)",
        "x = A::\\\nB %(on_linux)",
        "x = A:: B %(on_linux)",
    ] {
        assert_facts(block, false, false, None);
    }
}

/// A symbol spelled as an operator, or of a name ending in `=`, `?` or
/// `!`, is a value, so a `%`, `<<` or `/` after it opens nothing. A `:`
/// after a value ends a ternary.
#[test]
fn an_operator_symbol_is_a_value() {
    for symbol in [
        ":+", ":|", ":%", ":<<", ":==", ":a=", ":!~", ":<=>", ":[]", ":[]=", ":!", ":a?", ":a!",
        ":-", ":/", ":*", ":**", ":=~", ":+@", ":`",
    ] {
        assert_facts(&format!("x = {symbol} %(on_linux)"), true, false, None);
    }
    assert_facts("x = [:+, :-] %(on_linux)", true, false, None);
    assert_facts("f ? 1 :+ 2\n%(on_linux)", false, false, None);
}

/// The code inside `#{…}` is code, so its words count toward both facts,
/// in a string, a percent literal, a regex, a backtick command and a
/// heredoc body, and in a literal delimited by the end of its line. A
/// single-quoted literal holds no interpolation.
#[test]
fn the_code_of_an_interpolation_counts_toward_both_facts() {
    for block in [
        "x = \"#{on_linux}\"",
        "x = %(#{on_linux})",
        "x = /#{on_linux}/",
        "x = <<~EOS\n  #{on_linux}\nEOS",
        "x = %Q\n#{on_linux}\n",
        "x = `#{on_linux}`",
        "x = \"a#{b}#{on_linux}c\"",
        "x = \"#{\non_linux}\"",
        "x = \"#{\"#{on_linux}\"}\"",
    ] {
        assert_facts(block, true, false, None);
    }
    for block in ["x = '#{on_linux}'", "x = %q(#{on_linux})"] {
        assert_facts(block, false, false, None);
    }
    assert_facts(
        "depends_on macos: \"#{:ventura}\", linux: 1",
        false,
        true,
        None,
    );
    let edges = super::line_code_edges("x = \"#{on_linux}\"  \ny");
    assert_eq!(edges[0].trim_from, Some(17));
}

/// The code of each `#{…}` is a list of statements of its own: a `;` or
/// `$;` in it splits no statement around the literal, a `depends_on` that
/// opens one of its statements, on one line or several, counts, and the
/// names it assigns are bound in the scope around the literal, as Ruby
/// binds them.
#[test]
fn the_code_of_an_interpolation_is_read_as_statements_of_its_own() {
    for block in [
        "depends_on formula: \"#{a; b}\", macos: :ventura",
        "depends_on formula: \"x#{$;}\", macos: :ventura",
        "depends_on formula: \"x#{?;}\", macos: :ventura",
        "depends_on formula: $;, macos: :ventura",
        "x = <<~EOS\n  #{depends_on macos: :ventura}\nEOS",
        "x = \"#{depends_on macos: :ventura}\"",
        "x = \"#{a; depends_on macos: :ventura}\"",
        "x = \"#{\ndepends_on macos: :ventura}\"",
        "x = \"a#{b\ndepends_on macos: :ventura\n}\"",
        "x = \"#{\"#{(depends_on macos: :ventura)}\"}\"",
    ] {
        assert_facts(block, false, true, None);
    }
    assert_facts(
        "x = \"#{a}\"\ndepends_on formula: \"#{b} macos:\"",
        false,
        false,
        None,
    );
    for block in [
        "x = \"#{y = 1}\"\ny %(on_linux)",
        "x = \"#{y = 1; y %(on_linux)}\"",
        "x = \"#{y = 1\ny %(on_linux)}\"",
        "x = \"#{a, y = 1, 2}\"\ny %(on_linux)",
        "x = \"#{f(y = 1)}\"\ny %(on_linux)",
    ] {
        assert_facts(block, true, false, None);
    }
    for block in [
        "x = \"#{y}\"\ny %(on_linux)",
        "x = \"#{o.y = 1}\"\ny %(on_linux)",
        "x = \"#{y == 1}\"\ny %(on_linux)",
    ] {
        assert_facts(block, false, false, None);
    }
}

/// Straight after a name Ruby reads as a method call, a constant or a
/// name ending in `?` or `!` included, a `?` opens a character literal.
/// A `!` or `?` read as an
/// operator ends no word, so a `/` after it opens a regex.
#[test]
fn a_question_mark_after_a_method_name_and_a_slash_after_an_operator_open_literals() {
    for block in [
        "EOS!?\"/ on_linux do\nend\nputs$\"",
        "foo\n&.def??\"&.on_linux #p)\"",
        "foo?=\non_linux do\nend",
    ] {
        assert_facts(block, true, false, None);
    }
    for block in [
        "!/ on_linux do\nend\nBEGIN/#|a, (b, c)|z, w = 1?\\ /2case;",
        "x = !/ on_linux/",
        "x = a ? !/ on_linux/ : 1",
    ] {
        assert_facts(block, false, false, None);
    }
}

/// A `#{…}` that spans body lines of a heredoc is read on line after line,
/// as one in a string is, so the tag inside it closes nothing and the code
/// after the heredoc is read.
#[test]
fn an_interpolation_spanning_heredoc_body_lines_reads_on() {
    for block in [
        "x = <<~E\n  a #{b\n  }\nE\non_linux",
        "x = <<~E\n  a #{b\n  } c\nE\non_linux",
        "x = \"a #{b\n} c\"\non_linux",
        "x = <<~E\n  a #{\n  b }\nE\non_linux",
        "x = <<~E\n  #{\"\n  \"}\nE\non_linux",
        "x = <<~E\n  a #{b\n  c}\nE\non_linux",
        "x = <<~E\n  #{a\nE\n  }\nE\non_linux",
        "x = <<~A\n  #{<<~B\n  inner\n  B\n  }\nA\non_linux",
        "x = <<~A\n  #{y = 1\n  }\nA\ny %(on_linux)",
    ] {
        assert_facts(block, true, false, None);
    }
    assert_facts(
        "x = <<~E\n  #{depends_on macos: :x,\n  linux: 1}\nE",
        false,
        true,
        None,
    );
    assert_facts(
        "x = <<~E\n  #{a\n  }\n",
        false,
        false,
        unclosed(1, "heredoc"),
    );
}

/// The code inside `#{…}` is read the way the code around a literal is:
/// a name with a sigil, a symbol, a block or lambda parameter, a local
/// first assigned in a block and a `def` body bind nothing in the scope
/// around the literal.
#[test]
fn an_interpolation_binds_names_the_way_code_outside_a_literal_does() {
    for block in [
        "\"#{@a = 1}\"\na %(on_linux)",
        "\"#{@a = 1}\"; a %(on_linux)",
        "\"#{$a = 1}\"\na %(on_linux)",
        "\"#{:a=}\"\na %(on_linux)",
        "\"#{->(r) { s = r }}\"\ns %(on_linux)",
        "\"#{def g(p = 1) = p}\"\np %(on_linux)",
        "\"#{[1].map { |z| w = z; w }}\"\nw %(on_linux)",
        "\"#{def f; v = 1; end}\"\nv %(on_linux)",
        "\"#{x = 1 if (@q = 2)}\"\nq %(on_linux)",
        "\"#{1.then { |m| m }}\"\nm %(on_linux)",
        "x.then { |m| m }\nm %(on_linux)",
        "x&.if { |m| m }\nm %(on_linux)",
    ] {
        assert_facts(block, false, false, None);
    }
}

/// Every operator assignment binds its target wherever it stands: at the
/// head of a statement, after another `=`, in an argument, in a modifier's
/// condition and inside an interpolation. A target after `.` or with a
/// sigil binds no local.
#[test]
fn every_operator_assignment_binds_its_target() {
    let operators = [
        "||=", "&&=", "+=", "-=", "*=", "/=", "%=", "**=", "<<=", ">>=", "^=", "|=", "&=",
    ];
    for operator in operators {
        for head in [
            format!("b {operator} 1"),
            format!("a = b {operator} 1"),
            format!("f(b {operator} 1)"),
            format!("x = 1 if (b {operator} 1)"),
            format!("a, c = 1, 2 if b {operator} 1"),
            format!("\"#{{b {operator} 1}}\""),
            format!("\"#{{a = b {operator} 1}}\""),
        ] {
            assert_facts(&format!("{head}\nb %(on_linux)"), true, false, None);
        }
        for head in [
            format!("x = y.b {operator} 1"),
            format!("x = @b {operator} 1"),
        ] {
            assert_facts(&format!("{head}\nb %(on_linux)"), false, false, None);
        }
    }
    assert_facts("a <= 1\na %(on_linux)", false, false, None);
    assert_facts("x = a <= 1\na %(on_linux)", false, false, None);
}

/// Ruby reads `=begin` at the start of a line as a comment wherever the
/// line starts in code, the code of a `#{…}` left open on the line before
/// included, in a heredoc body or a string. The lines up to `=end` are
/// comment, so the `}` and heredoc tag on them close nothing.
#[test]
fn a_begin_comment_inside_an_open_interpolation_is_a_comment() {
    assert_facts(
        "x = <<~E\n  #{\n=begin\n  }\nE\n=end\n  1}\nE\non_linux do\nend",
        true,
        false,
        None,
    );
    assert_facts(
        "x = <<~E\n  #{\n=begin\n  }\nE\n=end\n  1}\n  on_linux\nE",
        false,
        false,
        None,
    );
    assert_facts("x = \"#{\n=begin\n}\"\n=end\n1}\"", false, false, None);
    assert_facts(
        "x = \"#{\n=begin\n}\"\n=end\n1}\"\non_linux do\nend",
        true,
        false,
        None,
    );
    let edges = super::line_code_edges("x = \"#{\n=begin\n}\" \n=end\n1}\"\ny");
    assert_eq!(
        edges.iter().map(|e| e.starts_in_code).collect::<Vec<_>>(),
        [true, false, false, false, false, true]
    );
    assert_eq!(
        edges.iter().map(|e| e.trim_from).collect::<Vec<_>>(),
        [None, None, None, None, Some(3), Some(0)]
    );
}

/// Ruby binds the named groups of a regex only when it has no `#{…}`.
#[test]
fn a_regex_with_an_interpolation_binds_no_named_group() {
    for head in ["/(?<v>#{1})/ =~ \"a\"", "/(?<v>.)#{}/ =~ \"a\""] {
        assert_facts(&format!("{head}\nz = v /on_linux/"), false, false, None);
    }
    assert_facts("/(?<v>.)/ =~ \"a\"\nz = v /on_linux/ 1", true, false, None);
}

/// A `%` after a label, the `:` of a ternary or a keyword that leaves an
/// operand opens a literal whatever delimits it, a `/` or `` ` `` after
/// that `:` opens one too, and a `:` written against a method call that
/// heads its statement or follows `.` or `&.` starts a symbol, as Ruby
/// reads them.
#[test]
fn a_percent_after_a_label_or_keyword_and_a_symbol_after_a_head_call() {
    for block in ["do1e5def:`% on_linux do\nend", "x = c ? 1 :?a\non_linux"] {
        assert_facts(block, true, false, None);
    }
    for block in [
        "%()if% \\on_linux ",
        " y!w wa:% on_linux do\nend",
        "\"a#{x}b\"in% =/a/?\\self.))on_linux ",
        "f(a:% on_linux )",
        "x = c ? 1 :% on_linux ",
        "x = c ? 1 :%(on_linux)",
        "x = c ? ?a:% on_linux ",
        "x = c ? 1 :/on_linux/",
        "x = c ? 1 :`on_linux`",
        "x = nil\nx&.a:/&&1#<=>/a/url\\%q(a(b)c)on_linux BEGINdefcaveats",
        "p foo:% on_linux ",
    ] {
        assert_facts(block, false, false, None);
    }
}

/// A line that goes on with the expression of the line before it, after a
/// `,` or an opening bracket, does not head a statement, so a `name:` at
/// its start is a label; after a line that ended a statement, or after a
/// `;` or a `#{`, the name heads one and takes no label.
#[test]
fn a_name_at_the_start_of_a_continued_expression_is_a_label() {
    for block in [
        "x = {\n  a:%w[on_linux],\n}",
        "f(\n  a:%w[on_linux])",
        "x = [\n  a:%w[on_linux]]",
        "x = {\n  a:/on_linux/,\n}",
        "f a,\n  b:%w[on_linux]",
        "x = {\n  # c\n\n  a:%w[on_linux],\n}",
    ] {
        assert_facts(block, false, false, None);
    }
    for block in [
        "x = 1\nfoo:`% on_linux do\nend",
        "x = 1;foo:`% on_linux do\nend",
        "x = \"#{foo:`% on_linux }\"",
    ] {
        assert_facts(block, true, false, None);
    }
}

/// A `=begin` that starts a heredoc body line is text of the body, so the
/// code after the heredoc is read.
#[test]
fn a_begin_marker_on_a_heredoc_body_line_is_body_text() {
    assert_facts("x = <<~E\n=begin\nE\non_linux do\nend", true, false, None);
    assert_facts("x = <<~E\n=begin\nE\n  on_linux\nE", true, false, None);
    let edges = super::line_code_edges("x = <<~E\n=begin\nE\non_linux do\nend");
    assert_eq!(
        edges.iter().map(|e| e.starts_in_code).collect::<Vec<_>>(),
        [true, false, true, true, true]
    );
}

/// Every keyword after which Ruby reads an operand, `%` written against
/// it or after a space opens a literal; `not` and `yield` read like a
/// method call, so a `%` after them is an operator.
#[test]
fn a_percent_after_every_keyword_that_leaves_an_operand_opens_a_literal() {
    for template in [
        "x = 1 and{} on_linux ",
        "begin{} on_linux \nend",
        "x.each do\nbreak{} on_linux \nend",
        "case{} on_linux \nwhen 1 then 2\nend",
        "x.each do{} on_linux \nend",
        "if x\n1\nelse{} on_linux \nend",
        "if x\n1\nelsif{} on_linux \n2\nend",
        "begin\n1\nensure{} on_linux \nend",
        "if{} on_linux \n1\nend",
        "for x in{} on_linux \nend",
        "x.each do\nnext{} on_linux \nend",
        "x = 1 or{} on_linux ",
        "begin\n1\nrescue{} on_linux \nend",
        "def f\nreturn{} on_linux \nend",
        "if x then{} on_linux \nend",
        "unless{} on_linux \nend",
        "until{} on_linux \nend",
        "case 1\nwhen{} on_linux \nend",
        "while{} on_linux \nend",
    ] {
        for percent in ["%", " %"] {
            assert_facts(&template.replace("{}", percent), false, false, None);
        }
    }
    for block in [
        "def f\nyield% on_linux \nend",
        "def f\nyield % on_linux \nend",
        "x = (not% on_linux )",
    ] {
        assert_facts(block, true, false, None);
    }
}

/// A line that ends a statement leaves the `name:` that starts the next
/// line heading a statement, so the name takes no label and its `` :` ``
/// is a symbol.
fn assert_next_name_heads(first: &str) {
    assert_facts(
        &format!("{first}\nfoo:`% on_linux do\nend"),
        true,
        false,
        None,
    );
}

/// A name spelled with `?` or `!` ends the line's statement.
#[test]
fn a_line_ending_in_a_name_with_a_question_or_exclamation_mark_ends_its_statement() {
    assert_next_name_heads("x = a.b?");
    assert_next_name_heads("x = a.b!");
}

/// The `|` that closes a block's parameters ends the line's statement.
#[test]
fn a_line_ending_in_the_bar_after_block_parameters_ends_its_statement() {
    for first in [
        "x.each { |y|",
        "x.each do |y|",
        "x.each { ||",
        "x.each { |y, z|",
    ] {
        assert_next_name_heads(first);
    }
}

#[test]
fn a_line_ending_in_a_closing_parenthesis_ends_its_statement() {
    assert_next_name_heads("x = f(1)");
}

#[test]
fn a_line_ending_in_a_closing_bracket_ends_its_statement() {
    assert_next_name_heads("x = [1]");
}

#[test]
fn a_line_ending_in_a_closing_brace_ends_its_statement() {
    assert_next_name_heads("x = {}");
}

#[test]
fn a_line_ending_in_a_literal_ends_its_statement() {
    assert_next_name_heads("x = \"a\"");
}

#[test]
fn a_line_ending_in_a_semicolon_ends_its_statement() {
    assert_next_name_heads("x = 1;");
}

/// A `#{` that ends its line starts a statement inside it, whatever the
/// line before left open, and the code of a heredoc body leaves the
/// statement around the heredoc as its opener left it.
#[test]
fn a_line_ending_in_an_open_interpolation_starts_a_statement_in_it() {
    for block in [
        "x = \"#{\nfoo:`% on_linux do\nend}\"",
        "x = \"#{y = 1\nfoo:`% on_linux do\nend}\"",
        "x = [1,\n \"#{\nfoo:`% on_linux do\nend}\"]",
        "x = <<~E\n  #{1}\nE\nfoo:`% on_linux do\nend",
    ] {
        assert_facts(block, true, false, None);
    }
    for block in [
        "x = \"#{f(1,\nfoo:%w[on_linux])}\"",
        "f(<<~E,\n  #{1}\nE\nfoo:%w[on_linux])",
    ] {
        assert_facts(block, false, false, None);
    }
}

/// A `;` starts a statement on a line that goes on with the expression of
/// the line before it.
#[test]
fn a_semicolon_on_a_continued_line_starts_a_statement() {
    assert_facts("x = [1,\n 2]; foo:`% on_linux do\nend", true, false, None);
}

/// A `#{` starts a statement on a line that goes on with the expression of
/// the line before it.
#[test]
fn an_interpolation_on_a_continued_line_starts_a_statement() {
    assert_facts(
        "x = [1,\n \"#{foo:`% on_linux do\nend}\"]",
        true,
        false,
        None,
    );
}

/// A line ending in `\\` goes on with its statement, so a `name:` on the
/// next line is a label.
#[test]
fn a_line_ending_in_a_backslash_reads_on() {
    assert_facts("f \\\nfoo:%w[on_linux]", false, false, None);
}

/// After a label a backtick opens a command string, as in a hash or an
/// argument list.
#[test]
fn a_backtick_after_a_label_opens_a_command_string() {
    for block in [
        "x = {a:`on_linux`}",
        "f(a:`on_linux`)",
        "f a:`on_linux`",
        "x = {\"a\":`on_linux`}",
        "x = {a:` on_linux`}",
    ] {
        assert_facts(block, false, false, None);
    }
}

/// Where an operand must stand, a `?` opens a character literal before
/// any character but a letter, digit or `_` followed by a word character,
/// so `?"` before a keyword is a literal and the keyword a modifier.
#[test]
fn a_question_mark_where_an_operand_stands_opens_a_quote_literal() {
    assert_facts("?\"if on_linux do\nend\n \"a#{x}b\" ", true, false, None);
    assert_facts(
        "?\"in#foo1.0defEOS:\"!%q(a(b)c)1.0==on_linux \nEOS",
        false,
        false,
        None,
    );
}

/// No local or parameter is spelled with a `?` or `!`, so a word ending in
/// one is a method call and an operator after it and a space opens a
/// literal; a `?` followed by a space is still a ternary.
#[test]
fn an_operator_after_a_name_ending_in_a_question_or_exclamation_mark_opens_a_literal() {
    assert_facts("y = foo? <<E\non_linux\nE", false, false, None);
    assert_facts("y = foo! %(on_linux)", false, false, None);
    assert_facts("y = foo? /on_linux/", false, false, None);
    assert_facts("y = x? ? 1 : 2\non_linux", true, false, None);
}

/// After `.`, `&.` or `::` a word is a method call whatever it spells, so
/// a `?` written straight after it opens a character literal.
#[test]
fn a_question_mark_after_a_method_named_like_a_keyword_opens_a_literal() {
    for block in [
        "y = foo.nil?= %(on_linux)",
        "y = foo.self?= %(on_linux)",
        "y = foo.true?= %(on_linux)",
        "y = foo&.end?= %(on_linux)",
        "y = foo.__FILE__?= %(on_linux)",
        "x = 1\ny = foo.x?= %(on_linux)",
    ] {
        assert_facts(block, true, false, None);
    }
    assert_facts("y = foo.end?=/ /on_linux/", false, false, None);
    assert_facts("y = foo.end?=/ + \"on_linux\"", false, false, None);
}
