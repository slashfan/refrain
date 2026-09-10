//! Le rendu. Ce module ne décide rien : il lit `App` et dessine.
//!
//! ratatui redessine **tout** l'écran à chaque image dans un tampon, puis
//! n'envoie au terminal que les cellules qui ont changé. On peut donc écrire
//! des fonctions de rendu totalement naïves sans que ça clignote.

use crate::app::{App, Tab};
use crate::parser::{Level, LogEntry};
use crate::stats::{self, StreamEntry, format_count, format_ms, format_time};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, List, ListItem, Paragraph, Row, Sparkline, Table, TableState,
    Tabs, Wrap,
};
use std::cmp::Reverse;

/// Ce que l'interface doit retenir d'une image sur l'autre : essentiellement le
/// défilement des tableaux, que ratatui gère pour nous via `TableState`.
#[derive(Default)]
pub struct UiState {
    errors: TableState,
    routes: TableState,
    nplus1: TableState,
}

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

pub fn draw(frame: &mut Frame, app: &App, ui: &mut UiState) {
    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_tabs(frame, app, tabs);

    match app.tab {
        Tab::Overview => draw_overview(frame, app, body),
        Tab::Errors => draw_errors(frame, app, ui, body),
        Tab::Endpoints => draw_endpoints(frame, app, ui, body),
        Tab::Sql => draw_sql(frame, app, ui, body),
        Tab::Stream => draw_stream(frame, app, body),
    }

    draw_footer(frame, app, footer);

    if app.show_help {
        draw_help(frame, frame.area());
    }
}

// ---------------------------------------------------------------------------
// Bandeaux
// ---------------------------------------------------------------------------

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let stats = &app.stats;
    let (peak, _) = stats.timeline.peak();
    let errors = stats.errors_total();

    let mut spans = vec![Span::styled(
        " refrain ",
        Style::new().fg(Color::Black).bg(ACCENT).bold(),
    )];

    // Le message transitoire passe devant tout le reste : sur un terminal
    // étroit, c'est la fin du bandeau qui est coupée, et un « écrit dans … »
    // qu'on ne voit pas ne sert à rien.
    if let Some(flash) = app.flash() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!(" {flash} "),
            Style::new().fg(Color::Black).bg(Color::Green).bold(),
        ));
    }

    spans.extend([
        Span::raw("  "),
        Span::styled(format_count(stats.total), Style::new().bold()),
        Span::styled(" lines", Style::new().fg(DIM)),
        sep(),
        Span::styled(
            format!("{:.0}/s", app.rate()),
            Style::new().fg(Color::Green),
        ),
        sep(),
        Span::styled(
            format!("peak {}/s", format_count(peak)),
            Style::new().fg(DIM),
        ),
        sep(),
        Span::styled(
            format_count(errors),
            Style::new().fg(if errors > 0 { Color::LightRed } else { DIM }),
        ),
        Span::styled(" errors", Style::new().fg(DIM)),
        sep(),
        Span::styled("durations: ", Style::new().fg(DIM)),
        Span::styled(stats.duration.label(), Style::new().fg(ACCENT)),
    ]);

    let open = stats.tracker.open_count();
    if open > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!("{open} open req."),
            Style::new().fg(DIM),
        ));
    }
    if stats.skipped > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!("{} skipped", format_count(stats.skipped)),
            Style::new().fg(Color::Yellow),
        ));
    }
    // Une fenêtre active se signale, sans quoi un écran vide laisserait croire
    // que les logs se sont taris.
    if stats.windowed() {
        spans.push(sep());
        spans.push(Span::styled(
            format!("{} out of window", format_count(stats.out_of_window)),
            Style::new().fg(DIM),
        ));
    }
    if app.all_sources_done() {
        spans.push(sep());
        spans.push(Span::styled(
            "end of stream",
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(failure) = app.failures.first() {
        spans.push(sep());
        spans.push(Span::styled(
            format!("⚠ {failure}"),
            Style::new().fg(Color::LightRed).bold(),
        ));
    }
    if let Some(endpoint) = &app.focus {
        spans.push(sep());
        spans.push(Span::styled("following ", Style::new().fg(DIM)));
        spans.push(Span::styled(
            stats::truncate(endpoint, 28),
            Style::new().fg(Color::Black).bg(ACCENT).bold(),
        ));
    }
    if app.frozen {
        spans.push(sep());
        spans.push(Span::styled(
            " FROZEN ",
            Style::new().fg(Color::Black).bg(Color::Yellow).bold(),
        ));
    }

    frame.render_widget(Line::from(spans), area);
}

fn sep() -> Span<'static> {
    Span::styled("  ·  ", Style::new().fg(DIM))
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, tab)| format!(" {} {} ", i + 1, tab.title()));

    let tabs = Tabs::new(titles)
        .select(Tab::ALL.iter().position(|t| *t == app.tab))
        .style(Style::new().fg(DIM))
        .highlight_style(Style::new().fg(Color::Black).bg(ACCENT).bold())
        .divider("");

    frame.render_widget(tabs, area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let hint = match app.tab {
        Tab::Endpoints => format!("s sort ({})", app.route_sort.label()),
        Tab::Sql => format!("N+1 threshold: {} ×", app.cli.nplus1),
        Tab::Stream => format!("/ search  ·  +/- level ≥ {}", app.min_level.as_str()),
        _ => "space freeze".to_string(),
    };
    let line = Line::from(vec![
        key("q"),
        Span::raw(" quit  "),
        key("Tab"),
        Span::raw(" tab  "),
        key("↑↓"),
        Span::raw(" move  "),
        key("r"),
        Span::raw(" reset  "),
        Span::styled(hint, Style::new().fg(DIM)),
        Span::raw("  "),
        key("?"),
        Span::raw(" help"),
    ]);
    frame.render_widget(line.style(Style::new().fg(DIM)), area);
}

fn key(k: &str) -> Span<'_> {
    Span::styled(k, Style::new().fg(ACCENT).bold())
}

/// Prend son titre par valeur et renvoie un `Block<'static>`.
///
/// Signer `block(title: &str) -> Block<'_>` lierait la durée de vie du bloc à
/// celle du titre : impossible alors de lui passer un `format!(…)`, dont le
/// résultat meurt à la fin de l'instruction.
fn block(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM))
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::new().fg(ACCENT).bold(),
        ))
}

// ---------------------------------------------------------------------------
// Onglet 1 — vue d'ensemble
// ---------------------------------------------------------------------------

fn draw_overview(frame: &mut Frame, app: &App, area: Rect) {
    let [volume, errors, bottom] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Min(4),
    ])
    .areas(area);

    draw_sparkline(frame, app, volume, false);
    draw_sparkline(frame, app, errors, true);

    let [levels, channels, top_errors] = Layout::horizontal([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(2, 4),
    ])
    .areas(bottom);

    draw_levels(frame, app, levels);
    draw_channels(frame, app, channels);
    draw_top_errors(frame, app, top_errors);
}

fn draw_sparkline(frame: &mut Frame, app: &App, area: Rect, errors_only: bool) {
    // Une colonne = une seconde : on demande exactement autant de seaux que le
    // bloc a de colonnes utiles.
    let width = area.width.saturating_sub(2) as usize;
    let data = if errors_only {
        app.stats.timeline.series(width, |b| b.errors)
    } else {
        app.stats.timeline.series(width, |b| b.total)
    };
    let max = data.iter().copied().max().unwrap_or(0);
    let title = if errors_only {
        format!("Errors / s  —  max {max} over {width} s")
    } else {
        format!("Volume / s  —  max {max} over {width} s")
    };

    let color = if errors_only {
        Color::LightRed
    } else {
        Color::Green
    };
    let sparkline = Sparkline::default()
        .block(block(title))
        .style(Style::new().fg(color))
        .data(data);

    frame.render_widget(sparkline, area);
}

fn draw_levels(frame: &mut Frame, app: &App, area: Rect) {
    let counts = &app.stats.by_level;
    let max = counts.iter().copied().max().unwrap_or(1).max(1);
    let bar_width = area.width.saturating_sub(20).max(1) as usize;

    let lines: Vec<Line> = Level::ALL
        .iter()
        .rev()
        .map(|level| {
            let count = counts[level.index()];
            // Règle de trois entre le compteur et la largeur disponible.
            let filled = (count as f64 / max as f64 * bar_width as f64).round() as usize;
            Line::from(vec![
                Span::styled(format!("{:<9}", level.as_str()), level_style(*level)),
                Span::styled(format!("{:>8} ", format_count(count)), Style::new().fg(DIM)),
                Span::styled("█".repeat(filled), level_style(*level)),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).block(block("Levels")), area);
}

fn draw_channels(frame: &mut Frame, app: &App, area: Rect) {
    let mut channels: Vec<_> = app.stats.channels.iter().collect();
    channels.sort_unstable_by_key(|(_, stat)| Reverse(stat.count));

    let items: Vec<ListItem> = channels
        .into_iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|(name, stat)| {
            let mut spans = vec![
                Span::styled(
                    format!("{:>8} ", format_count(stat.count)),
                    Style::new().fg(DIM),
                ),
                Span::raw(stats::truncate(name, 18)),
            ];
            if stat.errors > 0 {
                spans.push(Span::styled(
                    format!("  ({} err)", stat.errors),
                    Style::new().fg(Color::LightRed),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    frame.render_widget(List::new(items).block(block("Channels")), area);
}

fn draw_top_errors(frame: &mut Frame, app: &App, area: Rect) {
    if app.error_rows.is_empty() {
        let message = Paragraph::new("No errors so far. 🎉")
            .style(Style::new().fg(Color::Green))
            .block(block("Top errors"));
        frame.render_widget(message, area);
        return;
    }

    let width = area.width.saturating_sub(20) as usize;
    let items: Vec<ListItem> = app
        .error_rows
        .iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|row| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>7} × ", format_count(row.count)),
                    Style::new().fg(DIM),
                ),
                Span::styled(format!("{:<5}", row.level.short()), level_style(row.level)),
                Span::raw(stats::truncate(&row.signature, width.max(10))),
            ]))
        })
        .collect();

    frame.render_widget(List::new(items).block(block("Top errors")), area);
}

// ---------------------------------------------------------------------------
// Onglet 2 — erreurs
// ---------------------------------------------------------------------------

fn draw_errors(frame: &mut Frame, app: &App, ui: &mut UiState, area: Rect) {
    let [list, detail] = Layout::vertical([Constraint::Min(5), Constraint::Length(10)]).areas(area);

    let header = Row::new(vec!["Count", "Level", "Channel", "Last", "Signature"])
        .style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows = app.error_rows.iter().map(|row| {
        Row::new(vec![
            Cell::from(format_count(row.count)).style(Style::new().bold()),
            Cell::from(row.level.as_str()).style(level_style(row.level)),
            Cell::from(stats::truncate(&row.channel, 12)),
            Cell::from(format_time(row.last_seen)).style(Style::new().fg(DIM)),
            Cell::from(row.signature.clone()),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(13),
            Constraint::Length(9),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(block(match &app.focus {
        Some(endpoint) => format!(
            "Errors of {} ({} signatures)",
            endpoint,
            app.error_rows.len()
        ),
        None => format!("Errors grouped ({} signatures)", app.error_rows.len()),
    }))
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)).bold())
    .highlight_symbol("▌");

    ui.errors.select(Some(app.error_sel));
    frame.render_stateful_widget(table, list, &mut ui.errors);

    draw_error_detail(frame, app, detail);
}

fn draw_error_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(row) = app.error_rows.get(app.error_sel) else {
        frame.render_widget(
            Paragraph::new("Pick an error with ↑ ↓.")
                .style(Style::new().fg(DIM))
                .block(block("Detail")),
            area,
        );
        return;
    };
    // Le détail complet vit dans `Stats` : on ne le recopie pas à chaque tick.
    let Some(stat) = app.stats.errors.get(&row.signature) else {
        return;
    };

    let mut lines = vec![Line::from(vec![
        Span::styled("seen ", Style::new().fg(DIM)),
        Span::styled(format_count(stat.count), Style::new().bold()),
        Span::styled(" times  ·  from ", Style::new().fg(DIM)),
        Span::raw(format_time(stat.first_seen)),
        Span::styled(" to ", Style::new().fg(DIM)),
        Span::raw(format_time(stat.last_seen)),
    ])];

    if let Some(exception) = &stat.exception {
        lines.push(Line::from(vec![
            Span::styled("exception  ", Style::new().fg(DIM)),
            Span::styled(exception.clone(), Style::new().fg(Color::LightRed)),
        ]));
    }
    if let Some(endpoint) = &stat.endpoint {
        lines.push(Line::from(vec![
            Span::styled("endpoint   ", Style::new().fg(DIM)),
            Span::styled(endpoint.clone(), Style::new().fg(ACCENT)),
        ]));
    }
    lines.push(Line::from(""));
    for line in stat.message.lines().take(3) {
        lines.push(Line::from(line.to_string()));
    }
    if let Some(context) = &stat.context {
        lines.push(Line::from(""));
        for line in context.lines().take(6) {
            lines.push(Line::styled(line.to_string(), Style::new().fg(DIM)));
        }
    }

    let detail = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block("Latest occurrence"));
    frame.render_widget(detail, area);
}

// ---------------------------------------------------------------------------
// Onglet 3 — endpoints
// ---------------------------------------------------------------------------

fn draw_endpoints(frame: &mut Frame, app: &App, ui: &mut UiState, area: Rect) {
    let timed: usize = app.route_rows.iter().filter(|r| r.timed > 0).count();

    if app.route_rows.is_empty() {
        frame.render_widget(no_endpoints_help(), area);
        return;
    }

    let header = Row::new(vec![
        "Endpoint", "Requests", "SQL/req", "p50", "p95", "max", "Err.",
    ])
    .style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows = app.route_rows.iter().map(|row| {
        let error_style = if row.error_rate > 0.05 {
            Style::new().fg(Color::LightRed).bold()
        } else if row.errors > 0 {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().fg(DIM)
        };
        let (p50, p95, max) = if row.timed > 0 {
            (format_ms(row.p50), format_ms(row.p95), format_ms(row.max))
        } else {
            ("—".into(), "—".into(), "—".into())
        };

        let followed = app.focus.as_deref() == Some(row.name.as_str());
        Row::new(vec![
            Cell::from(if followed {
                format!("▸ {}", row.name)
            } else {
                row.name.clone()
            })
            .style(if followed {
                Style::new().fg(ACCENT).bold()
            } else {
                Style::new()
            }),
            Cell::from(format_count(row.requests)).style(Style::new().fg(DIM)),
            Cell::from(if row.avg_queries > 0.0 {
                format!("{:.1}", row.avg_queries)
            } else {
                "—".into()
            })
            .style(if row.avg_queries >= 20.0 {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(DIM)
            }),
            Cell::from(p50),
            Cell::from(p95).style(latency_style(row.p95, row.timed)),
            Cell::from(max).style(Style::new().fg(DIM)),
            Cell::from(if row.requests > 0 {
                format!("{:.1}%", row.error_rate * 100.0)
            } else {
                "—".into()
            })
            .style(error_style),
        ])
    });

    let title = format!(
        "Endpoints — sort: {} — {timed}/{} timed — source: {}",
        app.route_sort.label(),
        app.route_rows.len(),
        app.stats.duration.label()
    );

    let table = Table::new(
        rows,
        [
            Constraint::Min(22),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(7),
        ],
    )
    .header(header)
    .block(block(title))
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)).bold())
    .highlight_symbol("▌");

    ui.routes.select(Some(app.route_sel));
    frame.render_stateful_widget(table, area, &mut ui.routes);
}

/// Le tri par p95 n'a de sens que si quelque chose est chronométré : quand rien
/// ne l'est, mieux vaut expliquer comment y remédier que d'afficher un tableau
/// de tirets.
fn no_endpoints_help() -> Paragraph<'static> {
    let lines = vec![
        Line::from(""),
        Line::styled(
            "  No endpoint identified yet.",
            Style::new().fg(Color::Yellow).bold(),
        ),
        Line::from(""),
        Line::from(
            "  refrain spots a request from the \"Matched route\" line of the request channel,",
        ),
        Line::from("  and measures its duration in one of two ways:"),
        Line::from(""),
        Line::from(vec![
            Span::styled("    1. ", Style::new().fg(ACCENT)),
            Span::raw("a duration field in the context (duration_ms, elapsed…);"),
        ]),
        Line::from(vec![
            Span::styled("    2. ", Style::new().fg(ACCENT)),
            Span::raw("failing that, the gap between the first and last line of one"),
        ]),
        Line::from("       request, spotted by a token (Monolog's UidProcessor)."),
        Line::from(""),
        Line::styled(
            "  See \"Measuring durations\" in the README for the Symfony setup.",
            Style::new().fg(DIM),
        ),
    ];
    Paragraph::new(lines).block(block("Endpoints"))
}

fn latency_style(p95: f32, timed: u64) -> Style {
    if timed == 0 {
        Style::new().fg(DIM)
    } else if p95 >= 1000.0 {
        Style::new().fg(Color::LightRed).bold()
    } else if p95 >= 300.0 {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::Green)
    }
}

// ---------------------------------------------------------------------------
// Onglet 4 — SQL et motifs N+1
// ---------------------------------------------------------------------------

fn draw_sql(frame: &mut Frame, app: &App, ui: &mut UiState, area: Rect) {
    if app.nplus1_rows.is_empty() {
        // Un endpoint suivi qui n'a aucun N+1, ce n'est pas la même chose
        // qu'une détection en panne : l'aide de configuration égarerait.
        match &app.focus {
            Some(endpoint) => frame.render_widget(nothing_for_focus(endpoint, "N+1 pattern"), area),
            None => frame.render_widget(no_nplus1_help(app), area),
        }
        return;
    }

    let [list, detail] = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).areas(area);

    let header = Row::new(vec!["Endpoint", "Worst", "Avg.", "Requests", "SQL query"])
        .style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows = app.nplus1_rows.iter().map(|row| {
        Row::new(vec![
            Cell::from(row.endpoint.clone()),
            Cell::from(format!("{} ×", row.max_count)).style(severity_style(row.max_count)),
            Cell::from(format!("{:.0} ×", row.avg_count)).style(Style::new().fg(DIM)),
            Cell::from(format_count(row.requests)).style(Style::new().fg(DIM)),
            Cell::from(row.sql.clone()),
        ])
    });

    let title = match &app.focus {
        Some(endpoint) => format!(
            "N+1 patterns of {} — {} found — threshold: {} ×",
            endpoint,
            app.nplus1_rows.len(),
            app.cli.nplus1
        ),
        None => format!(
            "N+1 patterns — {} found — threshold: {} executions within one HTTP request",
            app.nplus1_rows.len(),
            app.cli.nplus1
        ),
    };

    let table = Table::new(
        rows,
        [
            // Largeur fixe pour l'endpoint : c'est le SQL qu'on veut lire en
            // entier, donc c'est à lui d'absorber la place restante. Deux
            // contraintes `Min` se la partageraient à parts égales.
            Constraint::Length(30),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Min(30),
        ],
    )
    .header(header)
    .block(block(title))
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)).bold())
    .highlight_symbol("▌");

    ui.nplus1.select(Some(app.nplus1_sel));
    frame.render_stateful_widget(table, list, &mut ui.nplus1);

    draw_sql_detail(frame, app, detail);
}

fn draw_sql_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(row) = app.nplus1_rows.get(app.nplus1_sel) else {
        return;
    };
    let Some(motif) = app.stats.nplus1.get(&row.key) else {
        return;
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("endpoint  ", Style::new().fg(DIM)),
            Span::styled(motif.endpoint.clone(), Style::new().fg(ACCENT)),
        ]),
        Line::from(vec![
            Span::styled("worst     ", Style::new().fg(DIM)),
            Span::styled(
                format!("{} executions", motif.max_count),
                severity_style(motif.max_count),
            ),
            Span::styled(
                format!(
                    "   ·   {:.1} on average over {} requests   ·   last {}",
                    motif.avg_count(),
                    format_count(motif.requests),
                    format_time(motif.last_seen)
                ),
                Style::new().fg(DIM),
            ),
        ]),
        Line::from(""),
        Line::styled(motif.sql.clone(), Style::new().fg(Color::White)),
    ];

    let detail = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block("Repeated query"));
    frame.render_widget(detail, area);
}

/// Trois situations très différentes se cachent derrière « pas de N+1 » : rien à
/// signaler, pas de SQL journalisé, ou pas de token pour regrouper les lignes.
/// Les confondre laisserait l'utilisateur croire que tout va bien.
/// L'écran d'un onglet vidé par le suivi d'un endpoint, plutôt que par
/// l'absence de données : la nuance change ce qu'il y a à faire.
fn nothing_for_focus(endpoint: &str, quoi: &str) -> Paragraph<'static> {
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  No "),
            Span::raw(quoi.to_string()),
            Span::raw(" for "),
            Span::styled(endpoint.to_string(), Style::new().fg(ACCENT).bold()),
            Span::raw("."),
        ]),
        Line::from(""),
        Line::styled(
            "  Esc releases the follow and brings the other endpoints back.",
            Style::new().fg(DIM),
        ),
    ];
    Paragraph::new(lines).block(block(format!("SQL — {endpoint}")))
}

fn no_nplus1_help(app: &App) -> Paragraph<'static> {
    let correlated = app.stats.tracker.key.is_some();
    let shapes = app.stats.sql_shapes();
    let mut lines = vec![Line::from("")];

    if !correlated {
        lines.push(Line::styled(
            "  Detection impossible: no request identifier found.",
            Style::new().fg(Color::Yellow).bold(),
        ));
        lines.push(Line::from(""));
        lines.push(Line::from(
            "  Spotting an N+1 means knowing which lines belong to the same HTTP",
        ));
        lines.push(Line::from(
            "  request. That takes a token shared by all of its lines:",
        ));
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "    # config/services.yaml",
            Style::new().fg(DIM),
        ));
        lines.push(Line::styled("    services:", Style::new().fg(ACCENT)));
        lines.push(Line::styled(
            "        Monolog\\Processor\\UidProcessor:",
            Style::new().fg(ACCENT),
        ));
        lines.push(Line::styled(
            "            tags: [monolog.processor]",
            Style::new().fg(ACCENT),
        ));
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "  Or point at the key you already log: --correlate-key my_key",
            Style::new().fg(DIM),
        ));
    } else if shapes == 0 {
        lines.push(Line::styled(
            "  No SQL query in the logs.",
            Style::new().fg(Color::Yellow).bold(),
        ));
        lines.push(Line::from(""));
        lines.push(Line::from(
            "  refrain spots queries through the context's `sql` field, the one Doctrine",
        ));
        lines.push(Line::from(
            "  writes on the `doctrine` channel at DEBUG level.",
        ));
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "  In production that level is often filtered out — which is exactly where",
            Style::new().fg(DIM),
        ));
        lines.push(Line::styled(
            "  an N+1 hides. A handler dedicated to `doctrine` makes it visible.",
            Style::new().fg(DIM),
        ));
    } else {
        lines.push(Line::styled(
            "  No N+1 pattern found. 🎉",
            Style::new().fg(Color::Green).bold(),
        ));
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "  {shapes} SQL query shapes seen, none repeated {} times or more",
            app.cli.nplus1
        )));
        lines.push(Line::from("  within a single HTTP request."));
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "  To be stricter: --nplus1 5",
            Style::new().fg(DIM),
        ));
    }

    Paragraph::new(lines).block(block("N+1"))
}

fn severity_style(count: u32) -> Style {
    if count >= 50 {
        Style::new().fg(Color::Red).bold()
    } else if count >= 20 {
        Style::new().fg(Color::LightRed)
    } else {
        Style::new().fg(Color::Yellow)
    }
}

// ---------------------------------------------------------------------------
// Onglet 5 — flux
// ---------------------------------------------------------------------------

fn draw_stream(frame: &mut Frame, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    let width = area.width.saturating_sub(2) as usize;

    // On parcourt le tampon de la fin vers le début : les entrées récentes sont
    // celles qui intéressent, et ça évite de filtrer tout l'historique.
    let visible: Vec<&StreamEntry> = app
        .stats
        .recent
        .iter()
        .rev()
        .filter(|entry| app.stream_shows(entry))
        .skip(app.stream_offset)
        .take(height)
        .collect();

    let items: Vec<ListItem> = visible
        .into_iter()
        .rev()
        .map(|item| ListItem::new(stream_line(&item.entry, width)))
        .collect();

    let mut title = match &app.focus {
        Some(endpoint) => format!(
            "Stream of {} — level ≥ {}",
            endpoint,
            app.min_level.as_str()
        ),
        None => format!("Stream — level ≥ {}", app.min_level.as_str()),
    };
    if app.searching {
        // Le curseur montre que la frappe suivante ira au motif, pas aux
        // raccourcis — c'est ce qui distingue les deux modes à l'écran.
        title.push_str(&format!(" — search: {}▌", app.search));
    } else if !app.search.is_empty() {
        title.push_str(&format!(" — « {} »", app.search));
    }
    if app.stream_offset > 0 {
        title.push_str(&format!(" — scrolled back {} lines", app.stream_offset));
    }

    frame.render_widget(List::new(items).block(block(title)), area);
}

fn stream_line(entry: &LogEntry, width: usize) -> Line<'static> {
    let prefix = 9 + 5 + entry.channel.chars().count().min(14) + 3;
    let room = width.saturating_sub(prefix).max(10);
    // Les entrées multi-lignes (stack traces) sont réduites à leur première
    // ligne : le détail complet reste consultable dans l'onglet Erreurs.
    let message = entry.message.lines().next().unwrap_or_default();

    Line::from(vec![
        Span::styled(format!("{} ", format_time(entry.ts)), Style::new().fg(DIM)),
        Span::styled(
            format!("{:<5}", entry.level.short()),
            level_style(entry.level),
        ),
        Span::styled(
            format!("{:<14} ", stats::truncate(&entry.channel, 14)),
            Style::new().fg(Color::Blue),
        ),
        Span::raw(stats::truncate(message, room)),
    ])
}

// ---------------------------------------------------------------------------
// Aide
// ---------------------------------------------------------------------------

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered(64, 24, area);
    // `Clear` efface la zone avant de dessiner par-dessus, sinon le contenu de
    // l'onglet transparaîtrait entre les caractères.
    frame.render_widget(Clear, popup);

    let rows = [
        ("q", "quit"),
        ("Esc", "drop the current filter, otherwise quit"),
        ("Enter", "follow the selected endpoint (Endpoints, SQL)"),
        ("Tab, ← →", "previous / next tab"),
        ("1 … 5", "jump straight to a tab"),
        ("↑ ↓, j k", "move through the list"),
        ("Page ↑ ↓", "move by blocks of 10"),
        ("g / G", "start / end of list"),
        ("space", "freeze or resume the stream"),
        ("s", "change the endpoint sort"),
        ("/", "search the stream (Esc clears)"),
        ("+ / -", "raise / lower the stream level"),
        ("w", "write the selection to a file"),
        ("y", "copy the selection to the clipboard"),
        ("r", "reset the counters"),
        ("?", "show this help"),
    ];

    let mut lines = vec![Line::from("")];
    for (keys, description) in rows {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{keys:<12}"), Style::new().fg(ACCENT).bold()),
            Span::raw(description),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled("  Any key closes this.", Style::new().fg(DIM)));

    frame.render_widget(Paragraph::new(lines).block(block("Shortcuts")), popup);
}

/// Centre un rectangle de taille fixe dans une zone.
fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn level_style(level: Level) -> Style {
    match level {
        Level::Debug => Style::new().fg(DIM),
        Level::Info => Style::new().fg(Color::White),
        Level::Notice => Style::new().fg(Color::Cyan),
        Level::Warning => Style::new().fg(Color::Yellow),
        Level::Error => Style::new().fg(Color::LightRed),
        Level::Critical => Style::new().fg(Color::Red).bold(),
        Level::Alert => Style::new().fg(Color::Magenta).bold(),
        Level::Emergency => Style::new().fg(Color::White).bg(Color::Red).bold(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::cli::Cli;
    use crate::event::Event;
    use crate::parser::parse_line;
    use clap::Parser;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app_avec_donnees() -> App {
        let mut app = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        let lignes = [
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {"route":"app_home","request_uri":"https://x.test/","method":"GET"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.050000+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT 1"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.100000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boom: "nope" at /var/www/src/X.php line 12 {"exception":"[object] (App\\Exception\\Boom(code: 0): nope at /var/www/src/X.php:12)"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.120000+02:00] request.INFO: Request finished {"route":"app_home","method":"GET","status":500,"duration_ms":120.0} {"token":"aaa"}"#,
        ];
        for ligne in lignes {
            app.stats
                .ingest(0, parse_line(ligne).expect("ligne valide"));
        }
        // Un N+1 franc, pour que l'onglet SQL ait quelque chose à montrer.
        let sql = r#"[2026-09-09T10:00:00.060000+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT t0.id FROM address t0 WHERE t0.customer_id = ?","params":{"1":1}} {"token":"aaa"}"#;
        for _ in 0..14 {
            app.stats
                .ingest(0, parse_line(sql).expect("ligne SQL valide"));
        }
        app.stats.finalize();
        // Le Tick construit les tableaux triés que l'affichage consomme.
        app.on_event(Event::Tick);
        app
    }

    /// Dessine dans un terminal virtuel et renvoie le texte brut de l'écran.
    fn rendu(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut state = UiState::default();
        terminal.draw(|frame| draw(frame, app, &mut state)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn chaque_onglet_affiche_ce_qu_on_attend() {
        let mut app = app_avec_donnees();

        app.tab = Tab::Overview;
        let vue = rendu(&app, 140, 40);
        assert!(vue.contains("refrain"), "le bandeau doit être là");
        assert!(vue.contains("doctrine"), "les canaux doivent apparaître");
        assert!(vue.contains("Boom"), "l'erreur doit remonter dans le top");

        app.tab = Tab::Errors;
        let vue = rendu(&app, 140, 40);
        assert!(vue.contains("CRITICAL"));
        // L'exception n'a pas de contexte de route : refrain la rattache à
        // « app_home » grâce au token partagé avec la ligne « Matched route ».
        assert!(vue.contains("app_home"), "erreur rattachée à son endpoint");

        app.tab = Tab::Endpoints;
        let vue = rendu(&app, 140, 40);
        assert!(vue.contains("app_home"));
        assert!(vue.contains("120 ms"), "la durée mesurée doit s'afficher");

        app.tab = Tab::Sql;
        let vue = rendu(&app, 140, 40);
        assert!(vue.contains("14 ×"), "la pire répétition doit s'afficher");
        assert!(
            vue.contains("FROM address"),
            "la requête fautive doit s'afficher"
        );

        app.tab = Tab::Stream;
        let vue = rendu(&app, 140, 40);
        assert!(vue.contains("Matched route"));
    }

    #[test]
    fn le_filtre_de_niveau_du_flux_fonctionne() {
        let mut app = app_avec_donnees();
        app.tab = Tab::Stream;

        assert!(rendu(&app, 140, 40).contains("Executing statement"));
        app.min_level = Level::Error;
        let vue = rendu(&app, 140, 40);
        assert!(
            !vue.contains("Executing statement"),
            "DEBUG doit être filtré"
        );
        assert!(vue.contains("Boom"), "CRITICAL doit rester");
    }

    fn touche(app: &mut App, code: KeyCode) {
        app.on_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn la_recherche_du_flux_filtre_sans_quitter() {
        let mut app = app_avec_donnees();
        app.tab = Tab::Overview;

        touche(&mut app, KeyCode::Char('/'));
        assert!(app.searching, "« / » ouvre la saisie");
        assert!(app.tab == Tab::Stream, "et emmène au flux");

        // Une lettre du motif ne doit pas déclencher son raccourci : « q »
        // quitterait, « r » remettrait les compteurs à zéro.
        touche(&mut app, KeyCode::Char('q'));
        touche(&mut app, KeyCode::Char('r'));
        assert!(!app.should_quit, "« q » saisi ne quitte pas");
        assert_eq!(app.stats.total, 18, "« r » saisi ne remet pas à zéro");
        touche(&mut app, KeyCode::Backspace);
        touche(&mut app, KeyCode::Backspace);
        assert!(app.search.is_empty());

        // La casse ne compte pas : le canal « doctrine » répond à « DoCtRiNe ».
        for c in "DoCtRiNe".chars() {
            touche(&mut app, KeyCode::Char(c));
        }
        let vue = rendu(&app, 140, 40);
        assert!(
            vue.contains("Executing statement"),
            "le canal doit répondre"
        );
        assert!(
            !vue.contains("Matched route"),
            "le reste du flux doit disparaître"
        );
        assert!(vue.contains("DoCtRiNe"), "le motif saisi doit s'afficher");

        // Entrée valide : le filtre reste, les raccourcis reviennent.
        touche(&mut app, KeyCode::Enter);
        assert!(!app.searching);
        assert!(rendu(&app, 140, 40).contains("Executing statement"));

        // Échap efface le motif et rend tout le flux.
        touche(&mut app, KeyCode::Char('/'));
        touche(&mut app, KeyCode::Esc);
        assert!(app.search.is_empty());
        assert!(!app.should_quit, "Échap pendant la saisie ne quitte pas");
        assert!(rendu(&app, 140, 40).contains("Matched route"));
    }

    #[test]
    fn la_recherche_porte_aussi_sur_l_endpoint_rattache() {
        let mut app = app_avec_donnees();
        app.tab = Tab::Stream;
        app.search = "app_home".to_string();
        let vue = rendu(&app, 140, 40);
        // Aucune de ces deux lignes ne contient « app_home » dans son message.
        // La première le porte dans son contexte de route ; la seconde, une
        // requête SQL de Doctrine, ne nomme rien du tout — c'est le token
        // partagé qui la rattache, et le flux s'en souvient.
        assert!(vue.contains("Request finished"));
        assert!(vue.contains("Executing statement"));
        // Un motif qui ne correspond à rien vide bien le flux.
        app.search = "app_checkout".to_string();
        assert!(!rendu(&app, 140, 40).contains("Request finished"));
    }

    /// Deux endpoints, chacun avec sa requête SQL, son erreur et son N+1 :
    /// de quoi vérifier que suivre l'un écarte vraiment l'autre.
    fn app_deux_endpoints() -> App {
        let mut app = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        let mut lignes = vec![
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {"route":"app_home"} {"token":"aaa"}"#.to_string(),
            r#"[2026-09-09T10:00:00.010000+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT 1 FROM home"} {"token":"aaa"}"#.to_string(),
            r#"[2026-09-09T10:00:00.020000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Maison: "cassé" at /var/www/src/H.php line 3 {"exception":"[object] (App\Exception\Maison(code: 0): cassé at /var/www/src/H.php:3)"} {"token":"aaa"}"#.to_string(),
            r#"[2026-09-09T10:00:01.000000+02:00] request.INFO: Matched route "app_search". {"route":"app_search"} {"token":"bbb"}"#.to_string(),
            r#"[2026-09-09T10:00:01.010000+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT 2 FROM search"} {"token":"bbb"}"#.to_string(),
            r#"[2026-09-09T10:00:01.020000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Recherche: "vide" at /var/www/src/S.php line 7 {"exception":"[object] (App\Exception\Recherche(code: 0): vide at /var/www/src/S.php:7)"} {"token":"bbb"}"#.to_string(),
        ];
        // Un N+1 pour chacun, pour que l'onglet SQL ait deux lignes à filtrer.
        for (token, table) in [("aaa", "adresse"), ("bbb", "facture")] {
            for _ in 0..12 {
                lignes.push(format!(
                    r#"[2026-09-09T10:00:0{}.500000+02:00] doctrine.DEBUG: Executing statement {{"sql":"SELECT t0.id FROM {} t0 WHERE t0.x = ?"}} {{"token":"{}"}}"#,
                    if token == "aaa" { 0 } else { 1 },
                    table,
                    token
                ));
            }
        }
        for ligne in &lignes {
            app.stats
                .ingest(0, parse_line(ligne).expect("ligne valide"));
        }
        app.stats.finalize();
        app.on_event(Event::Tick);
        app
    }

    #[test]
    fn suivre_un_endpoint_filtre_erreurs_sql_et_flux() {
        let mut app = app_deux_endpoints();

        // Sans suivi, les deux endpoints sont là.
        app.tab = Tab::Errors;
        let vue = rendu(&app, 140, 40);
        assert!(vue.contains("Maison") && vue.contains("Recherche"));

        // On suit app_home depuis le tableau des endpoints.
        app.tab = Tab::Endpoints;
        let position = app
            .route_rows
            .iter()
            .position(|r| r.name == "app_home")
            .expect("app_home doit être listé");
        app.route_sel = position;
        touche(&mut app, KeyCode::Enter);
        assert_eq!(app.focus.as_deref(), Some("app_home"));

        // Le tableau des endpoints, lui, garde tout le monde : c'est là qu'on
        // choisit. Mais celui qu'on suit est marqué.
        let vue = rendu(&app, 140, 40);
        assert!(vue.contains("app_search"), "les autres restent listés");
        assert!(vue.contains("▸ app_home"), "le suivi est marqué");
        assert!(vue.contains("following "), "et rappelé dans le bandeau");

        app.tab = Tab::Errors;
        let vue = rendu(&app, 140, 40);
        assert!(vue.contains("Maison"), "l'erreur de app_home reste");
        assert!(!vue.contains("Recherche"), "celle de app_search s'en va");

        app.tab = Tab::Sql;
        let vue = rendu(&app, 140, 40);
        assert!(vue.contains("adresse"), "le N+1 de app_home reste");
        assert!(!vue.contains("facture"), "celui de app_search s'en va");

        app.tab = Tab::Stream;
        let vue = rendu(&app, 140, 40);
        // L'exception ne nomme aucune route dans son contexte : si elle est
        // encore là, c'est bien que le token l'a rattachée à app_home.
        assert!(vue.contains("Maison"), "l'exception de app_home reste");
        assert!(!vue.contains("Recherche"), "celle de app_search s'en va");
        assert!(!vue.contains("app_search"), "ni sa ligne « Matched route »");

        // Entrée sur la même ligne relâche le suivi.
        app.tab = Tab::Endpoints;
        app.route_sel = position;
        touche(&mut app, KeyCode::Enter);
        assert!(app.focus.is_none());
        app.tab = Tab::Errors;
        assert!(rendu(&app, 140, 40).contains("Recherche"));
    }

    #[test]
    fn echap_defait_les_filtres_avant_de_quitter() {
        let mut app = app_deux_endpoints();
        app.focus = Some("app_home".to_string());
        app.search = "doctrine".to_string();

        touche(&mut app, KeyCode::Esc);
        assert!(app.search.is_empty(), "le motif part en premier");
        assert!(app.focus.is_some(), "le suivi tient encore");
        assert!(!app.should_quit);

        touche(&mut app, KeyCode::Esc);
        assert!(app.focus.is_none(), "puis le suivi");
        assert!(!app.should_quit);

        touche(&mut app, KeyCode::Esc);
        assert!(app.should_quit, "plus rien à défaire : on quitte");
    }

    #[test]
    fn survit_aux_terminaux_minuscules() {
        // Toute l'arithmétique de disposition doit être saturante : un terminal
        // ridiculement petit ne doit pas faire paniquer le programme.
        let mut app = app_avec_donnees();
        app.show_help = true;
        for (width, height) in [(1, 1), (12, 4), (20, 6), (40, 10), (300, 90)] {
            for tab in Tab::ALL {
                app.tab = tab;
                let _ = rendu(&app, width, height);
            }
        }
    }
}
