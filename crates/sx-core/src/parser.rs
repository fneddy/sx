use crate::error::ParseError;
use crate::handler_registry::HandlerRegistry;
use crate::query::{
    FileType, Filter, HandlerToken, HiddenMode, MatchExpr, Query, RangeExpr, SearchOptions,
    Timestamp,
};
use log::{debug, trace};
use std::path::PathBuf;
use std::str::FromStr;
use strum::EnumString;
use winnow::ascii::{alpha1, dec_uint, escaped, multispace1};
use winnow::combinator::{alt, cut_err, dispatch, opt, preceded, separated, terminated};
use winnow::error::{AddContext, ContextError, ErrMode, StrContext, StrContextValue};
use winnow::stream::{AsChar, Stream};
use winnow::token::{any, none_of, one_of, take_till, take_while};
use winnow::{ModalResult, Parser};

/// Cut-error label that signals an unknown namespace to `winnow_to_parse_error`.
const UNKNOWN_NS_LABEL: &str = "__unknown_namespace__";

#[derive(Debug, EnumString)]
#[strum(serialize_all = "snake_case")]
enum Namespace {
    #[strum(serialize = "name", serialize = "n")]
    Name,
    #[strum(serialize = "path", serialize = "p")]
    Path,
    #[strum(serialize = "hidden", serialize = "h")]
    Hidden,
    #[strum(serialize = "ext", serialize = "e")]
    Ext,
    #[strum(serialize = "content", serialize = "c")]
    Content,
    #[strum(serialize = "line", serialize = "l")]
    Line,
    #[strum(serialize = "size", serialize = "s")]
    Size,
    #[strum(serialize = "modified", serialize = "mod", serialize = "m")]
    Modified,
    #[strum(serialize = "created", serialize = "cr")]
    Created,
    #[strum(serialize = "accessed", serialize = "ac")]
    Accessed,
    #[strum(serialize = "type", serialize = "t")]
    Type,
    Mime,
    Perm,
    Owner,
    Group,
    Meta,
}

// ── Public entry points ────────────────────────────────────────────────────────

impl std::str::FromStr for Query {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Query::from_str_with_roots(s, vec![])
    }
}

impl Query {
    /// Convenience wrapper around [`Self::from_str_with_roots`] with no extra
    /// roots.  Equivalent to `s.parse::<Query>()` — both delegate to
    /// `from_str_with_roots` and neither is the canonical implementation of
    /// the other, so no circular delegation occurs.
    pub fn from_str(s: &str) -> Result<Query, ParseError> {
        Query::from_str_with_roots(s, vec![])
    }

    pub fn from_str_with_roots(input: &str, roots: Vec<PathBuf>) -> Result<Query, ParseError> {
        Query::from_str_with_registry(input, roots, None)
    }

    /// Parse a query string, routing non-file namespaces through `registry`.
    ///
    /// Known namespaces are stored as [`HandlerToken`]s; unknown ones return
    /// [`ParseError::UnknownNamespace`].  When `registry` is `None`, any
    /// non-file namespace is an immediate error.
    ///
    /// [`HandlerToken`]: crate::query::HandlerToken
    pub fn from_str_with_registry(
        input: &str,
        roots: Vec<PathBuf>,
        registry: Option<&HandlerRegistry>,
    ) -> Result<Query, ParseError> {
        debug!(
            "parser: parsing query {:?} with {} root(s)",
            input,
            roots.len()
        );

        if input.trim().is_empty() {
            return Err(ParseError::EmptyQuery);
        }

        let tokens = query_expr_with_registry
            .parse(input)
            .map_err(|e| winnow_to_parse_error(e, input))?;

        let global_namespaces: Vec<Box<[u8]>> = match registry {
            Some(reg) => reg.global_namespaces(),
            None => vec![],
        };

        let mut filters: Vec<Filter> = Vec::new();
        let mut handler_tokens: Vec<HandlerToken> = Vec::new();
        let mut bare_values: Vec<Box<[u8]>> = Vec::new();
        for tok in tokens {
            match tok {
                ParsedToken::Filter(f) => {
                    if let Filter::Or(ref children) = f {
                        if let [Filter::Name(ref expr), Filter::Content(_)] = children.as_slice() {
                            let val = String::from_utf8_lossy(&expr.value);
                            trace!("parser: bare term {:?}", val);
                            bare_values.push(expr.value.clone());
                        }
                    }
                    filters.push(f);
                }
                ParsedToken::Handler(ht) => {
                    let ns = String::from_utf8_lossy(&ht.namespace);
                    let val = String::from_utf8_lossy(&ht.value);
                    let known = match registry {
                        Some(reg) => reg.knows_namespace(&ht.namespace),
                        None => false,
                    };
                    if known {
                        debug!("parser: handler token {}:{:?}", ns, val);
                        handler_tokens.push(ht);
                    } else {
                        debug!("parser: unknown namespace {:?}", ns);
                        return Err(ParseError::UnknownNamespace {
                            token: ns.into_owned(),
                        });
                    }
                }
            }
        }
        if !bare_values.is_empty() && !global_namespaces.is_empty() {
            let combined: Box<[u8]> = bare_values
                .iter()
                .map(|v| String::from_utf8_lossy(v))
                .collect::<Vec<_>>()
                .join(" ")
                .into_bytes()
                .into_boxed_slice();
            for ns in &global_namespaces {
                handler_tokens.push(HandlerToken {
                    namespace: ns.clone(),
                    value: combined.clone(),
                });
            }
        }

        // Non-negated absolute paths promote to roots; relative/negated paths stay as filters.
        let mut extra_roots: Vec<PathBuf> = Vec::new();
        let filters: Vec<Filter> = filters
            .into_iter()
            .map(|f| promote_absolute_path_roots(f, &mut extra_roots))
            .collect();

        let mut merged_roots = roots;
        for dir in extra_roots {
            merged_roots.retain(|r| !r.starts_with(&dir) || *r == dir);
            let already_covered = merged_roots.iter().any(|r| dir == *r || dir.starts_with(r));
            if !already_covered {
                merged_roots.push(dir);
            }
        }

        let filters: Vec<Filter> = filters.into_iter().filter(|f| *f != Filter::Any).collect();
        let combined = match filters.len() {
            0 => Filter::Any,
            1 => filters.into_iter().next().unwrap(),
            _ => Filter::And(filters),
        };

        let (walk_filter, content_filter) = crate::query_builder::partition(combined);

        debug!(
            "parser: query built — {} root(s), {} handler token(s), {} bare term(s), walk={}, content={}",
            merged_roots.len(),
            handler_tokens.len(),
            bare_values.len(),
            if walk_filter == Filter::Any { "Any" } else { "Some" },
            if content_filter == Filter::Any { "Any" } else { "Some" },
        );

        Ok(Query {
            roots: merged_roots,
            walk_filter,
            content_filter,
            options: SearchOptions::default(),
            handler_tokens,
        })
    }
}

enum ParsedToken {
    Filter(Filter),
    Handler(HandlerToken),
}

fn query_expr_with_registry(input: &mut &str) -> ModalResult<Vec<ParsedToken>> {
    use winnow::ascii::multispace0;
    use winnow::combinator::delimited;
    delimited(
        multispace0,
        separated(1.., parsed_token, multispace1),
        multispace0,
    )
    .parse_next(input)
}

fn parsed_token(input: &mut &str) -> ModalResult<ParsedToken> {
    alt((
        or_combinator.map(ParsedToken::Filter),
        not_combinator.map(ParsedToken::Filter),
        namespace_token,
        bare_term.map(ParsedToken::Filter),
    ))
    .parse_next(input)
}

fn filter_token(input: &mut &str) -> ModalResult<Filter> {
    alt((or_combinator, not_combinator, namespace_filter, bare_term)).parse_next(input)
}

/// Variant of `filter_token` for inside `or(...)` where `,` is the argument separator.
fn filter_token_in_or(input: &mut &str) -> ModalResult<Filter> {
    alt((
        or_combinator,
        not_combinator,
        namespace_filter_in_or,
        bare_term_in_or,
    ))
    .parse_next(input)
}

fn or_combinator(input: &mut &str) -> ModalResult<Filter> {
    preceded(
        "or(",
        cut_err(
            terminated(separated(2.., filter_token_in_or, ',').map(Filter::Or), ')')
                .context(StrContext::Label("or combinator")),
        ),
    )
    .parse_next(input)
}

fn not_combinator(input: &mut &str) -> ModalResult<Filter> {
    preceded(
        "not(",
        cut_err(
            terminated(filter_token.map(|f| Filter::Not(Box::new(f))), ')')
                .context(StrContext::Label("not combinator")),
        ),
    )
    .parse_next(input)
}

/// Top-level `namespace:value` parser. Known file-search namespaces return a
/// `Filter`; unknown ones return a `Handler` for registry resolution.
fn namespace_token(input: &mut &str) -> ModalResult<ParsedToken> {
    let ns_str = terminated(
        alt((
            // multi-char word identifier
            take_while(1.., |c: char| c.is_alphanumeric() || c == '_'),
            // single symbolic character (e.g. "=")
            take_while(1, |c: char| {
                !c.is_alphanumeric() && c != ':' && !c.is_whitespace()
            }),
        )),
        ':',
    )
    .parse_next(input)?;

    match Namespace::from_str(ns_str) {
        Ok(ns) => cut_err(namespace_value_ctx(ns, false))
            .context(StrContext::Label("filter value"))
            .parse_next(input)
            .map(ParsedToken::Filter),
        Err(_) => {
            // Quoted string → strip the quotes, use the body verbatim.
            // Unquoted → consume until the next `word:` token or end of input,
            // so `weather:Jettingen, Germany` passes the full city name to the
            // handler instead of splitting at the space.
            let value: String = alt((
                quoted_string,
                handler_value_unquoted.map(str::to_string),
            ))
            .parse_next(input)?;
            Ok(ParsedToken::Handler(HandlerToken {
                namespace: Box::from(ns_str.as_bytes()),
                value: Box::from(value.trim().as_bytes()),
            }))
        }
    }
}

fn namespace_filter(input: &mut &str) -> ModalResult<Filter> {
    namespace_filter_impl(input, false)
}

fn namespace_filter_in_or(input: &mut &str) -> ModalResult<Filter> {
    namespace_filter_impl(input, true)
}

fn namespace_filter_impl(input: &mut &str, in_or: bool) -> ModalResult<Filter> {
    let ns_str = terminated(
        take_while(1.., |c: char| c.is_alphanumeric() || c == '_'),
        ':',
    )
    .parse_next(input)?;

    // `identifier:` consumed — unknown namespace is a hard Cut error.
    let ns = Namespace::from_str(ns_str).map_err(|_| {
        let chk = input.checkpoint();
        let ctx = ContextError::new().add_context(input, &chk, StrContext::Label(UNKNOWN_NS_LABEL));
        winnow::error::ErrMode::Cut(ctx)
    })?;

    cut_err(namespace_value_ctx(ns, in_or))
        .context(StrContext::Label("filter value"))
        .parse_next(input)
}

fn namespace_value_ctx(ns: Namespace, in_or: bool) -> impl FnMut(&mut &str) -> ModalResult<Filter> {
    move |input: &mut &str| match ns {
        Namespace::Name => match_expr.map(Filter::Name).parse_next(input),
        Namespace::Path => match_expr.map(Filter::Path).parse_next(input),
        Namespace::Hidden => hidden_value.parse_next(input),
        Namespace::Ext => {
            if in_or {
                ext_value_single.parse_next(input)
            } else {
                ext_value.parse_next(input)
            }
        }
        Namespace::Content => match_expr.map(Filter::Content).parse_next(input),
        Namespace::Line => match_expr.map(Filter::Line).parse_next(input),
        Namespace::Size => size_value.parse_next(input),
        Namespace::Modified => range_timestamp.map(Filter::Modified).parse_next(input),
        Namespace::Created => range_timestamp.map(Filter::Created).parse_next(input),
        Namespace::Accessed => range_timestamp.map(Filter::Accessed).parse_next(input),
        Namespace::Type => type_value.parse_next(input),
        Namespace::Mime => match_expr.map(Filter::Mime).parse_next(input),
        Namespace::Perm => perm_value.parse_next(input),
        Namespace::Owner => match_expr.map(Filter::Owner).parse_next(input),
        Namespace::Group => match_expr.map(Filter::Group).parse_next(input),
        Namespace::Meta => meta_value.parse_next(input),
    }
}

/// Consume the value for an unquoted handler token.
///
/// Reads everything up to (but not including) the first occurrence of
/// `<whitespace><identifier>:` — the start of the next namespace token —
/// then trims trailing whitespace from the result.  This lets the user write
/// `weather:Jettingen, Germany` without quotes while still allowing
/// `weather:berlin apps:firefox` to parse as two separate handler tokens.
fn handler_value_unquoted<'a>(input: &mut &'a str) -> ModalResult<&'a str> {
    use winnow::stream::Stream;
    let full = *input;

    if full.is_empty() {
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::new(),
        ));
    }

    // Walk through the string looking for the pattern \s+[A-Za-z0-9_]+:
    // which marks the start of the next handler/filter token.
    let end = next_namespace_start(full).unwrap_or(full.len());

    // Trim trailing whitespace (the spaces before the next token belong to
    // the separator, not the value).
    let value = full[..end].trim_end();
    if value.is_empty() {
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::new(),
        ));
    }

    input.next_slice(end);
    Ok(value)
}

/// Return the byte offset of the first `\s+[A-Za-z0-9_]+:` in `s`, or `None`.
///
/// Runs in O(n) via a single forward state-machine scan.
fn next_namespace_start(s: &str) -> Option<usize> {
    #[derive(Clone, Copy)]
    enum State {
        /// Not inside whitespace or a potential identifier.
        Noise,
        /// Inside a run of whitespace; `ws_start` is the offset of its first char.
        Whitespace(usize),
        /// Inside a word after whitespace; `ws_start` is where the whitespace began.
        Word(usize),
    }

    let mut state = State::Noise;
    for (i, c) in s.char_indices() {
        state = match state {
            State::Noise => {
                if c.is_whitespace() {
                    State::Whitespace(i)
                } else {
                    State::Noise
                }
            }
            State::Whitespace(ws) => {
                if c.is_whitespace() {
                    State::Whitespace(ws)
                } else if c.is_alphanumeric() || c == '_' {
                    State::Word(ws)
                } else {
                    State::Noise
                }
            }
            State::Word(ws) => {
                if c == ':' {
                    return Some(ws);
                } else if c.is_alphanumeric() || c == '_' {
                    State::Word(ws)
                } else if c.is_whitespace() {
                    State::Whitespace(i)
                } else {
                    State::Noise
                }
            }
        };
    }
    None
}

fn bare_term_filter(s: String) -> Filter {
    let expr = MatchExpr::exact(s.as_bytes());
    Filter::Or(vec![Filter::Name(expr.clone()), Filter::Content(expr)])
}

fn bare_term(input: &mut &str) -> ModalResult<Filter> {
    alt((
        quoted_string.map(bare_term_filter),
        take_while(1.., |c: char| !c.is_whitespace())
            .map(|s: &str| bare_term_filter(s.to_string())),
    ))
    .parse_next(input)
}

fn bare_term_in_or(input: &mut &str) -> ModalResult<Filter> {
    alt((
        quoted_string.map(bare_term_filter),
        take_while(1.., |c: char| !c.is_whitespace() && c != ')' && c != ',')
            .map(|s: &str| bare_term_filter(s.to_string())),
    ))
    .parse_next(input)
}

fn quoted_string(input: &mut &str) -> ModalResult<String> {
    // Recognised escape sequences: \" and \\.
    // Unknown escapes (e.g. \n, \t) are intentionally passed through as-is
    // (backslash + character), keeping the parser simple and predictable.
    let body = escaped(
        none_of(['"', '\\']),
        '\\',
        dispatch! { any;
            '"'  => winnow::combinator::empty.value('"'),
            '\\' => winnow::combinator::empty.value('\\'),
            c    => winnow::combinator::empty.map(move |_| c),
        },
    );
    preceded('"', terminated(body, '"')).parse_next(input)
}

fn match_expr(input: &mut &str) -> ModalResult<MatchExpr> {
    let negated = opt('!').parse_next(input)?.is_some();

    // Quoted string or unquoted token — `alt` tries quoted first, falls back.
    let val: String = alt((
        quoted_string,
        take_till(1.., |c: char| c.is_whitespace() || c == ')' || c == ',')
            .context(StrContext::Expected(StrContextValue::Description(
                "non-empty value",
            )))
            .map(str::to_string),
    ))
    .parse_next(input)?;

    Ok(MatchExpr {
        value: Box::from(val.as_bytes()),
        negated,
    })
}

#[derive(EnumString)]
#[strum(serialize_all = "snake_case")]
enum HiddenStr {
    #[strum(serialize = "true", serialize = "include")]
    Include,
    #[strum(serialize = "false", serialize = "exclude")]
    Exclude,
    Only,
}

fn hidden_value(input: &mut &str) -> ModalResult<Filter> {
    alpha1
        .verify_map(|s: &str| {
            HiddenStr::from_str(s).ok().map(|h| match h {
                HiddenStr::Include => Filter::Hidden(HiddenMode::Include),
                HiddenStr::Exclude => Filter::Hidden(HiddenMode::Exclude),
                HiddenStr::Only => Filter::Hidden(HiddenMode::Only),
            })
        })
        .context(StrContext::Expected(StrContextValue::Description(
            "true, false, include, exclude, or only",
        )))
        .parse_next(input)
}

/// `ext:rs,go,ts` — comma-separated list. Use `ext_value_single` inside `or(...)`.
fn ext_value(input: &mut &str) -> ModalResult<Filter> {
    separated(
        1..,
        take_while(1.., |c: char| {
            !c.is_whitespace() && c != ')' && c != ',' && c != ':'
        }),
        ',',
    )
    .map(|exts: Vec<&str>| Filter::Ext(exts.into_iter().map(|e| Box::from(e.as_bytes())).collect()))
    .parse_next(input)
}

fn ext_value_single(input: &mut &str) -> ModalResult<Filter> {
    take_while(1.., |c: char| {
        !c.is_whitespace() && c != ')' && c != ',' && c != ':'
    })
    .map(|e: &str| Filter::Ext(vec![Box::from(e.as_bytes())]))
    .parse_next(input)
}

fn size_value(input: &mut &str) -> ModalResult<Filter> {
    (range_modifier, size_bytes)
        .map(|(modifier, bytes)| Filter::Size(apply_range(modifier, bytes)))
        .parse_next(input)
}

fn size_bytes(input: &mut &str) -> ModalResult<u64> {
    let int_part: u64 = dec_uint.parse_next(input)?;
    let frac_part: Option<&str> =
        opt(('.', take_while(1.., AsChar::is_dec_digit)).map(|(_, digits): (_, &str)| digits))
            .parse_next(input)?;
    let unit: Option<&str> = opt(alpha1).parse_next(input)?;

    let mult: f64 = match unit.unwrap_or("").to_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_024.0,
        "m" | "mb" => 1_024.0 * 1_024.0,
        "g" | "gb" => 1_024.0 * 1_024.0 * 1_024.0,
        _ => {
            return Err(ErrMode::Cut(ContextError::new().add_context(
                input,
                &input.checkpoint(),
                StrContext::Expected(StrContextValue::Description(
                    "number with optional unit (b, k, m, g)",
                )),
            )));
        }
    };

    let bytes = if let Some(digits) = frac_part {
        let frac_f64 = digits
            .parse::<f64>()
            .map(|n| n / 10f64.powi(digits.len() as i32))
            .unwrap_or(0.0);
        // f64 has 53 bits of mantissa; u64::MAX needs 64 bits, so values above
        // ~2^53 bytes (~8 PiB) are rounded.  This is acceptable for a search
        // tool — no real filesystem holds such files — but noted here for clarity.
        let total = (int_part as f64 + frac_f64) * mult;
        if total >= u64::MAX as f64 {
            u64::MAX
        } else {
            total.round() as u64
        }
    } else {
        (int_part as f64 * mult).min(u64::MAX as f64) as u64
    };

    Ok(bytes)
}

// Default modifier semantics:
//   mod:7d           → Gt(7_days_ago)           — modified within the last 7 d
//   mod:today        → Between(today, tomorrow) — modified today
//   mod:2024-01-15   → Between(day_start, day_end)
//   mod:>7d / mod:<7d / mod:=2024-01-15 — explicit operators
fn range_timestamp(input: &mut &str) -> ModalResult<RangeExpr<Timestamp>> {
    // Peek before consuming so we can tell explicit `=` from the default `Eq`.
    let explicit_modifier = input.starts_with(['>', '<', '=']);
    let modifier = range_modifier.parse_next(input)?;
    let is_default = !explicit_modifier;

    alt((
        relative_duration_range(modifier, is_default),
        absolute_date_range(modifier, is_default),
        today_keyword_range(modifier, is_default),
    ))
    .parse_next(input)
}

fn relative_duration_range(
    modifier: RangeModifier,
    is_default: bool,
) -> impl FnMut(&mut &str) -> ModalResult<RangeExpr<Timestamp>> {
    move |input: &mut &str| {
        let (n, unit) = (dec_uint::<_, u64, _>, one_of(['d', 'w', 'm', 'y'])).parse_next(input)?;
        let ts = match unit {
            'd' => Timestamp::days_ago(n),
            'w' => Timestamp::weeks_ago(n),
            'm' => Timestamp::months_ago(n),
            'y' => Timestamp::months_ago(n * 12),
            _ => unreachable!(),
        };
        let range = if is_default {
            RangeExpr::Gt(ts)
        } else {
            apply_range(modifier, ts)
        };
        Ok(range)
    }
}

fn absolute_date_range(
    modifier: RangeModifier,
    is_default: bool,
) -> impl FnMut(&mut &str) -> ModalResult<RangeExpr<Timestamp>> {
    move |input: &mut &str| {
        let (y, _, m, _, d): (&str, _, &str, _, &str) = (
            take_while(4, AsChar::is_dec_digit),
            '-',
            take_while(2, AsChar::is_dec_digit),
            '-',
            take_while(2, AsChar::is_dec_digit),
        )
            .parse_next(input)?;

        let year: i32 = y.parse().map_err(|_| ErrMode::Cut(ContextError::new()))?;
        let month: u8 = m.parse().map_err(|_| ErrMode::Cut(ContextError::new()))?;
        let day: u8 = d.parse().map_err(|_| ErrMode::Cut(ContextError::new()))?;

        // Default: full-day range so `mod:2024-01-15` matches any file on that day.
        if is_default {
            let start = Timestamp::from_date(year, month, day)
                .map_err(|_| ErrMode::Cut(ContextError::new()))?;
            let end = Timestamp::end_of_date(year, month, day)
                .map_err(|_| ErrMode::Cut(ContextError::new()))?;
            return Ok(RangeExpr::Between(start, end));
        }

        let ts = Timestamp::from_date(year, month, day)
            .map_err(|_| ErrMode::Cut(ContextError::new()))?;
        Ok(apply_range(modifier, ts))
    }
}

fn today_keyword_range(
    modifier: RangeModifier,
    is_default: bool,
) -> impl FnMut(&mut &str) -> ModalResult<RangeExpr<Timestamp>> {
    move |input: &mut &str| {
        "today".parse_next(input)?;
        if is_default {
            return Ok(RangeExpr::Between(
                Timestamp::today_start(),
                Timestamp::tomorrow_start(),
            ));
        }
        Ok(apply_range(modifier, Timestamp::today_start()))
    }
}

#[derive(EnumString)]
enum FileTypeStr {
    #[strum(serialize = "f", serialize = "file")]
    File,
    #[strum(serialize = "d", serialize = "dir", serialize = "directory")]
    Dir,
    #[strum(serialize = "l", serialize = "symlink", serialize = "link")]
    Symlink,
    #[strum(serialize = "e", serialize = "empty")]
    Empty,
    #[strum(serialize = "x", serialize = "executable", serialize = "exec")]
    Executable,
    #[strum(serialize = "b", serialize = "binary")]
    Binary,
    #[strum(serialize = "t", serialize = "text")]
    Text,
}

fn type_value(input: &mut &str) -> ModalResult<Filter> {
    take_while(1.., |c: char| !c.is_whitespace() && c != ')')
        .verify_map(|s: &str| {
            FileTypeStr::from_str(s).ok().map(|ft| {
                Filter::Type(match ft {
                    FileTypeStr::File => FileType::File,
                    FileTypeStr::Dir => FileType::Dir,
                    FileTypeStr::Symlink => FileType::Symlink,
                    FileTypeStr::Empty => FileType::Empty,
                    FileTypeStr::Executable => FileType::Executable,
                    FileTypeStr::Binary => FileType::Binary,
                    FileTypeStr::Text => FileType::Text,
                })
            })
        })
        .context(StrContext::Expected(StrContextValue::Description(
            "f, d, l, e, x, b, or t",
        )))
        .parse_next(input)
}

fn meta_value(input: &mut &str) -> ModalResult<Filter> {
    // `meta:` is not yet implemented.  Fail immediately without consuming the
    // tag or value, so the error position points right at the unrecognised token.
    Err(ErrMode::Cut(ContextError::new().add_context(
        input,
        &input.checkpoint(),
        StrContext::Label("meta: filters are not yet implemented"),
    )))
}

/// `perm:644` / `perm:0644` — exact octal; `perm:+x` / `perm:+w` / `perm:+r` — has-bit.
fn perm_value(input: &mut &str) -> ModalResult<Filter> {
    let raw: &str =
        take_while(1.., |c: char| !c.is_whitespace() && c != ')' && c != ',').parse_next(input)?;

    if let Some(flag) = raw.strip_prefix('+') {
        let mask = match flag {
            "x" | "X" => 0o111u32,
            "w" | "W" => 0o222u32,
            "r" | "R" => 0o444u32,
            _ => {
                return Err(ErrMode::Cut(ContextError::new().add_context(
                    input,
                    &input.checkpoint(),
                    StrContext::Label("perm flag must be +x, +w, or +r"),
                )));
            }
        };
        return Ok(Filter::PermFlag(mask));
    }

    let octal_str = raw.strip_prefix('0').unwrap_or(raw);
    match u32::from_str_radix(octal_str, 8) {
        Ok(v) => Ok(Filter::PermOctal(v)),
        Err(_) => Err(ErrMode::Cut(ContextError::new().add_context(
            input,
            &input.checkpoint(),
            StrContext::Label("perm value must be an octal number (e.g. 644) or flag (+x, +w, +r)"),
        ))),
    }
}

#[derive(Clone, Copy)]
enum RangeModifier {
    Gt,
    Lt,
    Eq,
}

fn range_modifier(input: &mut &str) -> ModalResult<RangeModifier> {
    opt(one_of(['>', '<', '=']))
        .map(|c| match c {
            Some('>') => RangeModifier::Gt,
            Some('<') => RangeModifier::Lt,
            _ => RangeModifier::Eq,
        })
        .parse_next(input)
}

fn apply_range<T>(modifier: RangeModifier, value: T) -> RangeExpr<T> {
    match modifier {
        RangeModifier::Gt => RangeExpr::Gt(value),
        RangeModifier::Lt => RangeExpr::Lt(value),
        RangeModifier::Eq => RangeExpr::Eq(value),
    }
}

fn winnow_to_parse_error(
    e: winnow::error::ParseError<&str, ContextError>,
    input: &str,
) -> ParseError {
    use winnow::error::StrContext;

    let inner = e.inner();

    let has_unknown_ns = inner
        .context()
        .any(|c| matches!(c, StrContext::Label(l) if *l == UNKNOWN_NS_LABEL));

    if has_unknown_ns {
        let offset = input.len() - e.input().len();
        let consumed = &input[..offset];
        let ns = consumed
            .trim_end_matches(':')
            .rsplit(':')
            .next()
            .unwrap_or(consumed.trim_end_matches(':'))
            .to_string();
        return ParseError::UnknownNamespace { token: ns };
    }

    let is_combinator_error = inner
        .context()
        .any(|c| matches!(c, StrContext::Label(l) if l.ends_with("combinator")));

    if is_combinator_error {
        return ParseError::MalformedCombinator {
            raw: e.input().to_string(),
        };
    }

    let has_context = inner
        .context()
        .any(|c| matches!(c, StrContext::Label(_) | StrContext::Expected(_)));

    if has_context {
        ParseError::InvalidValue {
            namespace: String::new(),
            value: e.input().to_string(),
            reason: inner.to_string(),
        }
    } else {
        ParseError::MalformedCombinator {
            raw: e.input().to_string(),
        }
    }
}

/// Expand shell variables (`$VAR`, `${VAR}`) and a leading `~` in a path value.
///
/// Unknown variables are left as-is rather than erroring, so a script can pass
/// `p:$WORK_DIR` and an unset variable produces a literal `$WORK_DIR` substring
/// filter rather than an error.  Uses [`dirs::home_dir`] for `~` (correct on
/// all platforms).
fn expand_shell(s: &str) -> std::borrow::Cow<'_, str> {
    shellexpand::full_with_context_no_errors(
        s,
        // shellexpand needs Option<impl AsRef<str>>; HOME satisfies that directly.
        || std::env::var("HOME").ok(),
        |var| std::env::var(var).ok(),
    )
}

fn promote_absolute_path_roots(f: Filter, roots: &mut Vec<PathBuf>) -> Filter {
    match f {
        Filter::Path(ref expr) if !expr.negated => {
            if let Ok(s) = std::str::from_utf8(&expr.value) {
                let expanded = expand_shell(s);
                let p = std::path::Path::new(expanded.as_ref());
                if p.is_absolute() {
                    roots.push(p.to_path_buf());
                    return Filter::Any;
                }
                // Re-wrap the (possibly expanded) value if it changed but isn't absolute.
                if let std::borrow::Cow::Owned(ref owned) = expanded {
                    return Filter::Path(MatchExpr {
                        value: Box::from(owned.as_bytes()),
                        negated: false,
                    });
                }
            }
            f
        }
        Filter::And(children) => {
            let promoted: Vec<Filter> = children
                .into_iter()
                .map(|c| promote_absolute_path_roots(c, roots))
                .collect();
            let kept: Vec<Filter> = promoted.into_iter().filter(|c| *c != Filter::Any).collect();
            match kept.len() {
                0 => Filter::Any,
                1 => kept.into_iter().next().unwrap(),
                _ => Filter::And(kept),
            }
        }
        Filter::Or(children) => {
            let promoted: Vec<Filter> = children
                .into_iter()
                .map(|c| promote_absolute_path_roots(c, roots))
                .collect();
            // If *any* child became Any (always-true), the whole Or is trivially
            // true — an Or with a tautological arm always passes.
            if promoted.iter().any(|c| *c == Filter::Any) {
                Filter::Any
            } else {
                Filter::Or(promoted)
            }
        }
        // Negated path filter: expand ~ so the substring match works correctly.
        Filter::Path(ref expr) if expr.negated => {
            if let Ok(s) = std::str::from_utf8(&expr.value) {
                if let std::borrow::Cow::Owned(owned) = expand_shell(s) {
                    return Filter::Path(MatchExpr {
                        value: Box::from(owned.as_bytes()),
                        negated: true,
                    });
                }
            }
            f
        }
        other => other,
    }
}


#[cfg(test)]
mod handler_value_tests {
    use super::{next_namespace_start, query_expr_with_registry, ParsedToken};
    use winnow::Parser;

    fn handler_tokens(input: &str) -> Vec<(String, String)> {
        query_expr_with_registry
            .parse(input)
            .unwrap()
            .into_iter()
            .filter_map(|t| match t {
                ParsedToken::Handler(h) => Some((
                    String::from_utf8_lossy(&h.namespace).into_owned(),
                    String::from_utf8_lossy(&h.value).into_owned(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn single_word_handler() {
        let tokens = handler_tokens("weather:berlin");
        assert_eq!(tokens, [("weather".into(), "berlin".into())]);
    }

    #[test]
    fn multi_word_no_quotes() {
        let tokens = handler_tokens("weather:Jettingen, Germany");
        assert_eq!(tokens, [("weather".into(), "Jettingen, Germany".into())]);
    }

    #[test]
    fn multi_word_lowercase_no_quotes() {
        let tokens = handler_tokens("weather:jettingen, germany");
        assert_eq!(tokens, [("weather".into(), "jettingen, germany".into())]);
    }

    #[test]
    fn quoted_preserves_content() {
        let tokens = handler_tokens(r#"weather:"Jettingen, Germany""#);
        assert_eq!(tokens, [("weather".into(), "Jettingen, Germany".into())]);
    }

    #[test]
    fn two_handler_tokens_split_correctly() {
        let tokens = handler_tokens("weather:berlin apps:firefox");
        assert_eq!(
            tokens,
            [
                ("weather".into(), "berlin".into()),
                ("apps".into(), "firefox".into()),
            ]
        );
    }

    #[test]
    fn next_namespace_start_finds_word_colon() {
        assert_eq!(next_namespace_start("berlin apps:firefox"), Some(6));
        assert_eq!(next_namespace_start("berlin"), None);
        assert_eq!(next_namespace_start("Jettingen, Germany"), None);
    }
}
