//! The spellings and outcomes the "Bool-or-template values" table on the
//! templates docs page lists, so each crate that reads one of its rows can
//! check its reader against what the page says.

/// What a row's `auto` or "Any other render" cell says the field does, one
/// variant per wording the table uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentedOutcome {
    /// `true`: the field reads as true.
    True,
    /// `false`: the field reads as false.
    False,
    /// `false, with a warning`.
    FalseWithWarning,
    /// `true on a prerelease`.
    TrueOnPrerelease,
    /// `true on a snapshot`.
    TrueOnSnapshot,
    /// `GitHub decides (`legacy`)`: `make_latest: legacy` is sent.
    GitHubDecides,
    /// `an error`.
    Error,
}

impl DocumentedOutcome {
    /// The outcome a cell's text names. Only the text before a `;` is read,
    /// the rest being a note for the reader. Panics on a wording with no
    /// variant, so a reworded cell fails every test that reads it.
    pub fn from_cell(cell: &str) -> Self {
        let text = cell.split(';').next().unwrap_or_default().trim();
        match text {
            "true" => Self::True,
            "false" => Self::False,
            "false, with a warning" => Self::FalseWithWarning,
            "true on a prerelease" => Self::TrueOnPrerelease,
            "true on a snapshot" => Self::TrueOnSnapshot,
            "GitHub decides (`legacy`)" => Self::GitHubDecides,
            "an error" | "an error, an empty render included" => Self::Error,
            other => {
                panic!("the bool-or-template table names an outcome with no variant: {other:?}")
            }
        }
    }
}

/// One row of the "Bool-or-template values" table.
#[derive(Debug, Clone)]
pub struct DocumentedRow {
    /// The "Reads as true" spellings.
    pub truthy: Vec<String>,
    /// The "Reads as false" spellings.
    pub falsy: Vec<String>,
    /// The `auto` column.
    pub auto: DocumentedOutcome,
    /// The "Any other render" column.
    pub other: DocumentedOutcome,
}

/// The row whose first cell is `field` in the "Bool-or-template values"
/// table of `docs/site/content/docs/general/templates.md`, its columns found
/// by their header text.
///
/// Each spelling cell's backticked values are taken, with the word `empty`
/// standing for `""`, and every non-empty value is returned as written, in
/// upper case, capitalized and padded with spaces, since the page says each
/// is trimmed and read in any letter case. Panics when the section, a
/// column or the row is missing.
pub fn documented_bool_row(field: &str) -> DocumentedRow {
    let page = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/site/content/docs/general/templates.md"
    ))
    .expect("read the templates docs page");
    let section = page
        .split("\n## Bool-or-template values\n")
        .nth(1)
        .expect("the templates page has a `## Bool-or-template values` section");
    let section = section.split("\n## ").next().unwrap_or(section);
    let cells = |line: &str| -> Vec<String> {
        line.trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect()
    };
    let header = section
        .lines()
        .find(|line| line.starts_with("| Field |"))
        .map(cells)
        .expect("the bool-or-template table has a `| Field |` header");
    let column = |name: &str| {
        header
            .iter()
            .position(|h| h == name)
            .unwrap_or_else(|| panic!("the bool-or-template table has no {name:?} column"))
    };
    let (true_col, false_col, auto_col, other_col) = (
        column("Reads as true"),
        column("Reads as false"),
        column("`auto`"),
        column("Any other render"),
    );
    let row = section
        .lines()
        .filter(|line| line.starts_with('|'))
        .map(cells)
        .find(|row| row.first().is_some_and(|c| c == field))
        .unwrap_or_else(|| panic!("no `{field}` row in the bool-or-template table"));
    let cell = |col: usize| {
        row.get(col)
            .unwrap_or_else(|| panic!("the `{field}` row has no cell {col}"))
            .as_str()
    };
    let read = |cell: &str| {
        let mut out = Vec::new();
        for value in cell.split('`').skip(1).step_by(2) {
            let mut capitalized = value.to_string();
            capitalized[..1].make_ascii_uppercase();
            out.extend([
                value.to_string(),
                value.to_ascii_uppercase(),
                capitalized,
                format!("  {value} "),
            ]);
        }
        if cell.contains("empty") {
            out.push(String::new());
        }
        out
    };
    DocumentedRow {
        truthy: read(cell(true_col)),
        falsy: read(cell(false_col)),
        auto: DocumentedOutcome::from_cell(cell(auto_col)),
        other: DocumentedOutcome::from_cell(cell(other_col)),
    }
}
