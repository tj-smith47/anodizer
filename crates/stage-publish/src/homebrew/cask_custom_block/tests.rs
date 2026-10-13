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
/// one that stays open, and stops at a `#` outside them.
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
        ("\"a#{\"}\"}b\" # c", "\"\" "),
        ("\"a#{'\"'}b\" # c", "\"\" "),
        ("\"#{{a: 1}[:a]}\" x", "\"\" x"),
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
                open: false
            }],
            "{line}"
        );
    }
}

/// A literal or heredoc that does not close on its line carries on into
/// the following lines; its content is left out until it closes.
#[test]
fn an_open_literal_carries_into_the_next_lines() {
    let line = |text: &str, open: bool| CodeLine {
        text: text.to_string(),
        open,
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
            &[line("", true), line("", true), line("\"\" y", false)],
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
