//! `sxc` — TUI frontend binary.  Requires feature `tui`.

use std::time::Duration;
use std::{path::PathBuf, process, sync::Arc};

use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    DefaultTerminal, Frame,
};
use tokio::sync::mpsc;

use sx_core::{HandlerRegistry, Query};

use sxc::shared::app::App;
use sxc::shared::config::{resolve_roots, Tui as TuiConfig};
use sxc::shared::msg::{MetaValue, ResultMsg};
use sxc::shared::registry::build_registry;
use sxc::shared::search::{SearchHandle, SearchPhase};
use sxc::shared::wire::{n_workers, wire_search};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "sxc",
    about = "Universal query dispatch — file search, calc, sys, and more (TUI)",
    long_about = None,
)]
struct Cli {
    /// Additional plugin directories to scan for *.so plugins (may be repeated).
    #[arg(long = "plugin", value_name = "DIR")]
    plugin_dirs: Vec<PathBuf>,

    /// Print results to stdout and exit (no TUI). Useful for scripting.
    #[arg(long, short = 'p')]
    print: bool,

    /// Directory to search (defaults to the current directory).
    #[arg(long, short = 'd', value_name = "DIR")]
    path: Option<PathBuf>,

    /// Query tokens — all remaining arguments are joined with spaces.
    ///
    /// Example: sxc --path /home/eddy c:foo c:bar
    #[arg(trailing_var_arg = true, num_args = 1.., value_name = "QUERY")]
    query_tokens: Vec<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    env_logger::Builder::from_default_env()
        .filter_module("pdf_oxide", log::LevelFilter::Off)
        .init();

    let cli = Cli::parse();
    let config = sxc::shared::config::load_or_default();

    if cli.query_tokens.is_empty() {
        eprintln!("Usage: sxc [--plugin <dir>]... [--print|-p] [--path <dir>] <query>...");
        process::exit(1);
    }

    let registry = build_registry(&cli.plugin_dirs);
    let roots = resolve_roots(cli.path.clone(), &config);
    let query = build_query(&cli.query_tokens, roots, &registry);

    if cli.print {
        run_print(query, registry).await;
        return;
    }

    let terminal = ratatui::init();

    let (tx, rx) = mpsc::unbounded_channel::<ResultMsg>();
    let cq = query.compile_with_registry(Some(Arc::clone(&registry)));
    wire_search(&cq, n_workers(), tx.clone());
    drop(tx);

    let open_path = run_tui(terminal, rx, SearchHandle::new(cq), config.tui).await;
    ratatui::restore();

    // process::exit is intentional: the Tokio runtime drop would otherwise block
    // waiting for spawn_blocking plugin tasks (PDF, Office, …) to return from
    // their synchronous FFI dispatch.  The terminal is already restored and
    // SearchHandle::drop has already cancelled all workers, so nothing is skipped.
    let code = match open_path {
        Some(path) => match open::that(&path) {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("sxc: could not open '{path}': {e}");
                1
            }
        },
        None => 0,
    };
    process::exit(code);
}

fn build_query(tokens: &[String], roots: Vec<PathBuf>, registry: &Arc<HandlerRegistry>) -> Query {
    let query_str = tokens.join(" ");
    match Query::from_str_with_registry(&query_str, roots, Some(registry)) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("sxc: invalid query: {e}");
            process::exit(2);
        }
    }
}

async fn run_print(query: Query, registry: Arc<HandlerRegistry>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<ResultMsg>();
    let cq = query.compile_with_registry(Some(Arc::clone(&registry)));
    wire_search(&cq, n_workers(), tx.clone());
    drop(tx);

    while let Some(r) = rx.recv().await {
        let prefix = r.meta("icon_symbol").unwrap_or("");
        if let Some(t) = &r.title {
            if let Some(v) = r.meta("value") {
                println!("{}{}: {}  {}", prefix, r.namespace, t, v);
            } else {
                println!("{}{}: {}", prefix, r.namespace, t);
            }
        } else if let Some(v) = r.meta("value") {
            println!("{}{}: {}", prefix, r.namespace, v);
        }
        for e in &r.metadata {
            match &e.value {
                MetaValue::Text(v) => println!("  {}: {}", e.key, v),
                MetaValue::Number(n) => println!("  {}: {}", e.key, n),
                MetaValue::Bytes(b) => println!("  {}: <{} bytes>", e.key, b.len()),
            }
        }
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

const DIM: Style = Style::new().fg(Color::DarkGray);
const HIGHLIGHT: Style = Style::new()
    .bg(Color::DarkGray)
    .add_modifier(Modifier::BOLD);

/// Returns the number of visible result rows for a terminal of `total_height` rows.
///
/// The layout reserves the bottom half of the terminal for a future input area,
/// and 2 rows are consumed by the results-block border.
fn visible_rows(total_height: u16) -> usize {
    // Upper half (minus footer row), then subtract the 2-row block border.
    total_height
        .saturating_sub(1)
        .checked_div(2)
        .unwrap_or(0)
        .saturating_sub(2) as usize
}

/// Build one result row as a `Line<'static>`.
///
/// All spans own their string data (via `format!` / `.clone()`) so the returned
/// `Line` is `'static` — required by ratatui's widget rendering model.
fn result_line(r: &ResultMsg, selected: bool) -> Line<'static> {
    use sxc::shared::icon::symbol_for_result;
    let marker = if selected { "▶" } else { " " };
    let sym = symbol_for_result(r);
    let sym_prefix = sym.map(|s| format!("{s} ")).unwrap_or_default();
    let ns_span = Span::styled(format!("{marker} {sym_prefix}{}  ", r.namespace), DIM);

    let content_span = match (&r.title, r.meta("value")) {
        (Some(t), Some(v)) => Span::raw(format!("{t}  {v}")),
        (Some(t), None) => Span::raw(t.clone()),
        (None, Some(v)) => Span::styled(
            v.to_owned(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        (None, None) => {
            if let Some(e) = r.metadata.first() {
                match &e.value {
                    MetaValue::Text(v) => Span::raw(format!("{}: {}", e.key, v)),
                    MetaValue::Number(n) => Span::raw(format!("{}: {}", e.key, n)),
                    MetaValue::Bytes(b) => Span::raw(format!("{}: <{} bytes>", e.key, b.len())),
                }
            } else {
                Span::raw(String::new())
            }
        }
    };

    Line::from(vec![ns_span, content_span])
}

fn render(app: &App, phase: SearchPhase, selected: usize, scroll: usize, frame: &mut Frame) {
    let area = frame.area();

    // The lower half is reserved for a future query-input widget.
    let [results_area, footer_area] = Layout::vertical([
        Constraint::Max(area.height.saturating_sub(1) / 2),
        Constraint::Length(1),
    ])
    .areas(area);

    let key = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let status = match phase {
        SearchPhase::Running => Span::styled(" Searching… ", Style::default().fg(Color::Cyan)),
        SearchPhase::Done    => Span::styled(" Done ",       Style::default().fg(Color::Green)),
        SearchPhase::Stopped => Span::styled(" Stopped ",   Style::default().fg(Color::Yellow)),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            status,
            Span::styled("│ ", DIM),
            Span::styled("↑/k", key),
            Span::raw(" up  "),
            Span::styled("↓/j", key),
            Span::raw(" down  "),
            Span::styled("PgUp/PgDn", key),
            Span::raw(" page  "),
            Span::styled("Enter", key),
            Span::raw(" open  "),
            Span::styled("q/Esc", key),
            Span::raw(" quit"),
        ])),
        footer_area,
    );

    if app.results.is_empty() {
        return;
    }

    let block = Block::default().borders(Borders::ALL).title(" Results ");
    let inner = block.inner(results_area);
    frame.render_widget(block, results_area);

    for (row_y, (i, r)) in
        (inner.y..inner.bottom()).zip(app.results.iter().enumerate().skip(scroll))
    {
        let row = Rect::new(inner.x, row_y, inner.width, 1);
        if i == selected {
            frame.buffer_mut().set_style(row, HIGHLIGHT);
        }
        frame.render_widget(Paragraph::new(result_line(r, i == selected)), row);
    }
}

fn clamp_scroll(selected: usize, scroll: usize, visible: usize) -> usize {
    if selected < scroll {
        selected
    } else if selected >= scroll + visible {
        selected + 1 - visible
    } else {
        scroll
    }
}

// ── TUI loop state ────────────────────────────────────────────────────────────

struct TuiState {
    app: App,
    /// Search lifecycle — phase transitions drive the select guard and the footer.
    session: SearchHandle,
    selected: usize,
    scroll: usize,
    interval: tokio::time::Interval,
    /// TUI settings read from config (fps values, …).
    cfg: TuiConfig,
    /// Set on the first q/Esc press; the second press always exits.
    quit_armed: bool,
}

impl TuiState {
    fn new(session: SearchHandle, cfg: TuiConfig) -> Self {
        let mut interval =
            tokio::time::interval(Duration::from_secs_f32(1.0 / cfg.fps_searching));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            app: App::new(),
            session,
            selected: 0,
            scroll: 0,
            interval,
            cfg,
            quit_armed: false,
        }
    }

    /// Switch to the idle (low-fps) redraw rate.
    fn slow_down_interval(&mut self) {
        self.interval =
            tokio::time::interval(Duration::from_secs_f32(1.0 / self.cfg.fps_done));
        self.interval
            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    }

    /// Workers finished naturally — mark Done and drop to idle fps.
    fn finish_search(&mut self) {
        self.session.finish();
        self.slow_down_interval();
    }

    /// User stopped the search — mark Stopped, cancel workers, drop to idle fps.
    fn stop_search(&mut self) {
        self.session.stop();
        self.slow_down_interval();
    }

    /// Return the action target of the currently selected result, if any.
    fn selected_action(&self) -> Option<String> {
        self.app.results.get(self.selected).and_then(|r| {
            r.meta("file_path")
                .or_else(|| r.meta("gio_id"))
                .map(str::to_owned)
                .or_else(|| r.title.clone())
        })
    }
}

// ── main loop ─────────────────────────────────────────────────────────────────

async fn run_tui(
    mut terminal: DefaultTerminal,
    mut rx: mpsc::UnboundedReceiver<ResultMsg>,
    session: SearchHandle,
    cfg: TuiConfig,
) -> Option<String> {
    let mut s = TuiState::new(session, cfg);
    let mut events = EventStream::new();

    let _ = terminal.draw(|f| render(&s.app, s.session.phase(), s.selected, s.scroll, f));

    loop {
        let mut dirty = false;
        tokio::select! {
            _ = s.interval.tick() => {
                let before = s.app.results.len();
                s.app.flush_pending();
                if s.app.results.len() != before { dirty = true; }
            }

            msg = rx.recv(), if s.session.is_running() => {
                match msg {
                    Some(r) => { s.app.apply(r); }
                    None    => { s.finish_search(); }
                }
                s.app.flush_pending();
                dirty = true;
            }

            ev = events.next() => {
                match ev {
                    None => break None,
                    Some(Err(_)) => {}
                    Some(Ok(Event::Resize(..))) => dirty = true,
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        dirty = true;
                        let last = s.app.results.len().saturating_sub(1);
                        s.selected = s.selected.min(last);
                        match (key.code, key.modifiers) {
                            // Ctrl+C always quits immediately.
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) => break None,

                            // q / Esc — identical two-step exit:
                            //   press 1: stop the search if still running; arm the quit flag.
                            //   press 2: always exit, regardless of what press 1 did.
                            (KeyCode::Char('q'), KeyModifiers::NONE) | (KeyCode::Esc, _) => {
                                if s.quit_armed {
                                    break None;
                                }
                                s.quit_armed = true;
                                if s.session.is_running() {
                                    s.stop_search();
                                }
                            }

                            (KeyCode::Up,   _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                                s.selected = s.selected.saturating_sub(1);
                            }
                            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                                s.selected = (s.selected + 1).min(last);
                            }
                            (KeyCode::PageUp,   _) => s.selected = s.selected.saturating_sub(10),
                            (KeyCode::PageDown, _) => s.selected = (s.selected + 10).min(last),
                            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => s.selected = 0,
                            (KeyCode::End,  _) | (KeyCode::Char('G'), KeyModifiers::NONE) => s.selected = last,

                            (KeyCode::Enter, _) => break s.selected_action(),

                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
        }

        if dirty {
            s.selected = s.selected.min(s.app.results.len().saturating_sub(1));
            if terminal
                .draw(|f| {
                    let visible = visible_rows(f.area().height).max(1);
                    s.scroll = clamp_scroll(s.selected, s.scroll, visible);
                    render(&s.app, s.session.phase(), s.selected, s.scroll, f);
                })
                .is_err()
            {
                break None;
            }
        }
    }
}
