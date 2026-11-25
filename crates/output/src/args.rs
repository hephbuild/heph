//! The `output` clap group: `--json`, `--fields`, `--jq`.

use crate::jq::Jq;
use crate::write::{Printer, Style};
use crate::{Cancelled, Field, FieldId, Record, Selection};
use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser};
use futures::Stream;
use serde::Serialize;
use std::ffi::OsStr;
use std::io::{BufWriter, Stdout, Write};
use std::marker::PhantomData;

/// The output flags of one command, whose result is an `R` (or a list of
/// them). Flatten it into the command's clap struct:
///
/// ```ignore
/// #[command(flatten)]
/// out: output::JsonArgs<Row<'static>>,
/// ```
///
/// The three flags form one clap group, `output`, so another flag can
/// `conflicts_with = "output"`.
///
/// Parsing refuses (exit 2) a `--fields` that mixes replacing and adding
/// names, across repeated flags too: `deps,+driver` has no reading a user
/// would guess.
pub struct JsonArgs<R: Record + 'static> {
    json: bool,
    fields: Vec<FieldSel>,
    jq: Option<Jq>,
    _record: PhantomData<fn() -> R>,
}

// The derived half of `JsonArgs`: clap's parse, before the cross-value check
// in `JsonArgs::from_arg_matches` (a value parser sees one value at a time).
#[derive(clap::Args)]
#[group(id = "output", multiple = true)]
struct Flags<R: Record + 'static> {
    /// Print the result as JSON on stdout, and nothing else there
    ///
    /// A list-shaped command prints one array and a record-shaped one an
    /// object (`null` when there is nothing to report). Pretty on a terminal;
    /// piped, a record is one line and a list one element per line.
    #[arg(long, help_heading = "Output")]
    json: bool,

    /// The JSON fields to print (implies --json)
    ///
    /// `a,b` replaces the default set, `+a,+b` adds to it (a mix of the two
    /// is refused), `all` is every field, and `help` prints the field table. A field is computed only when
    /// it is selected; its tier says what it costs: `free` (already computed),
    /// `resolved` (fetches a spec or def), `built` (builds dependencies).
    #[arg(
        long,
        value_name = "FIELDS",
        value_delimiter = ',',
        value_parser = FieldsParser::<R>::new(),
        help_heading = "Output"
    )]
    fields: Vec<FieldSel>,

    /// Filter the JSON through a jq expression (implies --json)
    ///
    /// Each result is printed on its own line: a string raw, anything else
    /// as compact JSON. A filter that fails while it runs exits 2.
    #[arg(long, value_name = "EXPR", value_parser = parse_jq, help_heading = "Output")]
    jq: Option<Jq>,

    #[arg(skip)]
    _record: PhantomData<fn() -> R>,
}

impl<R: Record + 'static> clap::Args for JsonArgs<R> {
    fn group_id() -> Option<clap::Id> {
        Flags::<R>::group_id()
    }

    fn augment_args(cmd: clap::Command) -> clap::Command {
        Flags::<R>::augment_args(cmd)
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        Flags::<R>::augment_args_for_update(cmd)
    }
}

impl<R: Record + 'static> clap::FromArgMatches for JsonArgs<R> {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let Flags {
            json, fields, jq, ..
        } = Flags::<R>::from_arg_matches(matches)?;
        refuse_mixed_fields(R::FIELDS, &fields)?;
        Ok(Self {
            json,
            fields,
            jq,
            _record: PhantomData,
        })
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

/// Refuse a `--fields` whose names (over every repetition of the flag) are
/// partly bare, replacing the default set, and partly `+`, adding to it.
fn refuse_mixed_fields(fields: &[Field], sels: &[FieldSel]) -> Result<(), clap::Error> {
    // `(adds, name)` of each named field, in order.
    let named = || {
        sels.iter().filter_map(|sel| {
            let (add, i) = match *sel {
                FieldSel::Set(i) => (false, i),
                FieldSel::Add(i) => (true, i),
                FieldSel::All | FieldSel::Help(_) => return None,
            };
            fields.get(i).map(|f| (add, f.name))
        })
    };
    let first = |want_add: bool| named().find_map(|(add, name)| (add == want_add).then_some(name));
    let (Some(replacing), Some(adding)) = (first(false), first(true)) else {
        return Ok(());
    };
    let mut names: Vec<&str> = Vec::new();
    for (_, name) in named() {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let exactly = names.join(",");
    let added = names
        .iter()
        .map(|n| format!("+{n}"))
        .collect::<Vec<_>>()
        .join(",");
    Err(clap::Error::raw(
        clap::error::ErrorKind::ArgumentConflict,
        format!(
            "`--fields` mixes replacing (`{replacing}`) and adding (`+{adding}`) names; \
             write `--fields {exactly}` for exactly those fields, or `--fields {added}` \
             to add them to the default set\n"
        ),
    ))
}

impl<R: Record + 'static> Clone for JsonArgs<R> {
    fn clone(&self) -> Self {
        Self {
            json: self.json,
            fields: self.fields.clone(),
            jq: self.jq.clone(),
            _record: PhantomData,
        }
    }
}

impl<R: Record + 'static> std::fmt::Debug for JsonArgs<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonArgs")
            .field("json", &self.json)
            .field("fields", &self.fields)
            .field("jq", &self.jq)
            .finish()
    }
}

impl<R: Record + 'static> Default for JsonArgs<R> {
    fn default() -> Self {
        Self {
            json: false,
            fields: Vec::new(),
            jq: None,
            _record: PhantomData,
        }
    }
}

impl<R: Record + 'static> JsonArgs<R> {
    /// Parse the output flags alone, as a command would see them. For tests:
    /// `JsonArgs::<Row>::try_parse_from(["--fields", "addr"])`.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        use clap::{Args as _, FromArgMatches as _};
        let cmd = Self::augment_args(clap::Command::new("heph").no_binary_name(true));
        let matches = cmd.try_get_matches_from(args)?;
        Self::from_arg_matches(&matches)
    }

    /// Whether any of `--json`, `--fields`, `--jq` was given: the command
    /// prints its JSON document and nothing else on stdout, and keeps the TUI
    /// off.
    pub fn enabled(&self) -> bool {
        self.json || !self.fields.is_empty() || self.jq.is_some()
    }

    /// The selected fields: the default set, unless `--fields` named some
    /// (`+name` adds to the defaults, a bare name replaces them; parsing
    /// refused a mix of the two).
    pub fn selection(&self) -> Selection {
        let fields = R::FIELDS;
        let replaces = self.fields.iter().any(|f| matches!(f, FieldSel::Set(_)));
        let mut selection = if replaces {
            Selection::default()
        } else {
            Selection::defaults(fields)
        };
        for sel in &self.fields {
            match *sel {
                FieldSel::Set(i) | FieldSel::Add(i) => selection.insert(i),
                FieldSel::All => return Selection::all(fields),
                FieldSel::Help(_) => {}
            }
        }
        selection
    }

    /// Whether the command must compute `field`: compute a non-default
    /// field only when this says so (I4).
    pub fn wants(&self, field: FieldId<R>) -> bool {
        self.selection().contains(field.index())
    }

    /// The compiled `--jq` filter, if one was given.
    pub fn jq(&self) -> Option<&Jq> {
        self.jq.as_ref()
    }

    /// A printer on stdout, laid out for whatever stdout is (D5).
    pub fn printer(&self) -> Printer<'_, R, BufWriter<Stdout>> {
        self.printer_to(BufWriter::new(std::io::stdout()), Style::for_stdout())
    }

    /// A printer on `w`, laid out as `style`. For tests.
    pub fn printer_to<W: Write>(&self, w: W, style: Style) -> Printer<'_, R, W> {
        Printer::new(w, style, self.selection(), self.jq.as_ref())
    }

    /// [`Printer::record`] on stdout.
    pub fn record<T>(&self, value: &T, cancelled: Cancelled<'_>) -> anyhow::Result<()>
    where
        T: Record<Static = R::Static> + Serialize,
    {
        self.printer().record(value, cancelled)
    }

    /// [`Printer::null`] on stdout.
    pub fn null(&self, cancelled: Cancelled<'_>) -> anyhow::Result<()> {
        self.printer().null(cancelled)
    }

    /// [`Printer::list`] on stdout.
    pub async fn list<T, S>(&self, rows: S, cancelled: Cancelled<'_>) -> anyhow::Result<()>
    where
        T: Record<Static = R::Static> + Serialize,
        S: Stream<Item = anyhow::Result<T>>,
    {
        self.printer().list(rows, cancelled).await
    }

    /// [`Printer::list_iter`] on stdout.
    pub fn list_iter<T, I>(&self, rows: I, cancelled: Cancelled<'_>) -> anyhow::Result<()>
    where
        T: Record<Static = R::Static> + Serialize,
        I: IntoIterator<Item = T>,
    {
        self.printer().list_iter(rows, cancelled)
    }
}

/// One `--fields` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSel {
    /// `name`: the field at this index, replacing the default set.
    Set(usize),
    /// `+name`: the field at this index, added to the default set.
    Add(usize),
    /// `all`: every field.
    All,
    /// `help`: print the field table and exit 0 ([`fields_help`]).
    Help(&'static [Field]),
}

fn parse_jq(code: &str) -> Result<Jq, String> {
    Jq::compile(code)
}

/// `--fields`'s value parser: clap's `PossibleValuesParser` over `R::FIELDS`
/// (so suggestions, `--help` and completion come from clap), plus the `+`
/// prefix, `all` and `help`.
struct FieldsParser<R> {
    inner: PossibleValuesParser,
    _record: PhantomData<fn() -> R>,
}

impl<R> Clone for FieldsParser<R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _record: PhantomData,
        }
    }
}

impl<R: Record + 'static> FieldsParser<R> {
    fn new() -> Self {
        Self {
            inner: PossibleValuesParser::new(possible_values(R::FIELDS)),
            _record: PhantomData,
        }
    }
}

fn possible_values(fields: &'static [Field]) -> Vec<PossibleValue> {
    fields
        .iter()
        .map(|f| {
            let default = if f.default { ", default" } else { "" };
            PossibleValue::new(f.name).help(format!("[{}{default}] {}", f.tier.as_str(), f.doc))
        })
        .chain([
            PossibleValue::new("all").help("every field, built ones included"),
            PossibleValue::new("help").help("print the field table and exit"),
        ])
        .collect()
}

impl<R: Record + 'static> TypedValueParser for FieldsParser<R> {
    type Value = FieldSel;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &OsStr,
    ) -> Result<FieldSel, clap::Error> {
        let (add, name) = match value.to_str().and_then(|v| v.strip_prefix('+')) {
            Some(rest) => (true, OsStr::new(rest)),
            None => (false, value),
        };
        // Validates (suggesting the nearest name on a typo) and refuses a
        // non-UTF-8 value.
        let name = self.inner.parse_ref(cmd, arg, name)?;
        Ok(match name.as_str() {
            "all" => FieldSel::All,
            "help" => FieldSel::Help(R::FIELDS),
            name => {
                let index = R::FIELDS
                    .iter()
                    .position(|f| f.name == name)
                    .ok_or_else(|| clap::Error::new(clap::error::ErrorKind::InvalidValue))?;
                if add {
                    FieldSel::Add(index)
                } else {
                    FieldSel::Set(index)
                }
            }
        })
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        self.inner.possible_values()
    }
}

/// When the parsed command line asked for `--fields help`, the field table to
/// print on stdout before exiting 0. `main` calls this right after parsing,
/// before any engine work.
///
/// clap cannot print it itself: a value parser's `DisplayHelp` error is
/// rendered with an `error:` prefix.
pub fn fields_help(matches: &clap::ArgMatches) -> Option<String> {
    let mut leaf = matches;
    let mut path = Vec::new();
    while let Some((name, sub)) = leaf.subcommand() {
        path.push(name);
        leaf = sub;
    }
    let fields = leaf
        .try_get_many::<FieldSel>("fields")
        .ok()
        .flatten()?
        .find_map(|sel| match sel {
            FieldSel::Help(fields) => Some(*fields),
            _ => None,
        })?;
    Some(field_table(&path.join(" "), fields))
}

/// The table `--fields help` prints.
fn field_table(command: &str, fields: &[Field]) -> String {
    let name_width = fields
        .iter()
        .map(|f| f.name.len())
        .chain([5])
        .max()
        .unwrap_or(5);
    let mut out = format!("Fields of `heph {command} --json` (* = printed by default):\n\n");
    out.push_str(&format!(
        "  {:<name_width$}  {:<8}  DESCRIPTION\n",
        "FIELD", "TIER"
    ));
    for f in fields {
        let mark = if f.default { "*" } else { " " };
        out.push_str(&format!(
            "{mark} {:<name_width$}  {:<8}  {}\n",
            f.name,
            f.tier.as_str(),
            f.doc
        ));
    }
    out.push_str(
        "\n`--fields a,b` prints exactly those, `--fields +a` adds to the default set, \
         `--fields all` prints every field.\n\
         Tiers: free (already computed), resolved (fetches a spec or def), \
         built (builds dependencies).\n",
    );
    out
}

#[cfg(test)]
mod tests;
