//! The rendering. This module decides nothing: it reads `App` and draws.
//!
//! ratatui redraws the **whole** screen into a buffer on every frame, then
//! sends the terminal only the cells that changed. So rendering functions can
//! be written completely naively without anything flickering.

use crate::app::{App, Tab};
use crate::parser::{Level, LogEntry};
use crate::stats::{self, StreamEntry, format_count, format_ms, format_time};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, HighlightSpacing, List, ListItem, Paragraph, Row, Sparkline,
    Table, TableState, Tabs, Wrap,
};

/// What the interface must remember from one frame to the next: essentially
/// the scrolling of the tables, which ratatui handles for us through `TableState`.
#[derive(Default)]
pub struct UiState {
    errors: TableState,
    routes: TableState,
    commands: TableState,
    nplus1: TableState,
    outbound: TableState,
    messages: TableState,
    deprecations: TableState,
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
        Tab::Outbound => draw_outbound(frame, app, ui, body),
        Tab::Messenger => draw_messenger(frame, app, ui, body),
        Tab::Deprecations => draw_deprecations(frame, app, ui, body),
        Tab::Stream => draw_stream(frame, app, body),
    }

    draw_footer(frame, app, footer);

    if app.show_help {
        draw_help(frame, frame.area());
    }
}

// ---------------------------------------------------------------------------
// Banners
// ---------------------------------------------------------------------------

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let stats = &app.stats;
    let (peak, _) = stats.timeline.peak();
    let errors = stats.errors_total();

    let mut spans = vec![Span::styled(
        " refrain ",
        Style::new().fg(Color::Black).bg(ACCENT).bold(),
    )];

    // The transient message goes before everything else: on a narrow terminal
    // it is the end of the banner that gets cut, and a "written to …" nobody
    // sees is of no use.
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

    // Deprecations are logged at INFO: nothing else in the banner would
    // betray them, and a quiet upgrade-blocker is what this tab is for.
    if stats.deprecations_total > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!("{} deprecations", format_count(stats.deprecations_total)),
            Style::new().fg(Color::Yellow),
        ));
    }
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
    // A reached ceiling says so: without it the endpoint table would silently
    // become incomplete, and a missing route would read as a route receiving
    // nothing.
    if stats.capped.any() {
        spans.push(sep());
        spans.push(Span::styled(
            format!("capped: {}", stats.capped.names().join(", ")),
            Style::new().fg(Color::Yellow),
        ));
    }
    // An active window announces itself, otherwise an empty screen would
    // suggest the logs had gone quiet.
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

/// Takes its title by value and returns a `Block<'static>`.
///
/// Signing `block(title: &str) -> Block<'_>` would tie the block's lifetime to
/// the title's: it would then be impossible to hand it a `format!(…)`, whose
/// result dies at the end of the statement.
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
// Tab 1 — overview
// ---------------------------------------------------------------------------

/// One block of the overview's bottom row, and its share of the width.
type OverviewBlock = (fn(&mut Frame, &App, Rect), u32);

fn draw_overview(frame: &mut Frame, app: &App, area: Rect) {
    let [volume, errors, bottom] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Min(4),
    ])
    .areas(area);

    draw_sparkline(frame, app, volume, false);
    draw_sparkline(frame, app, errors, true);

    // Two blocks always, the others only when the logs carry what they show:
    // Monolog writes no status of its own, and not every application has a
    // cache, so an empty frame would take a fifth of the row to say nothing.
    // Built as a list rather than as a branch per combination — there are four
    // of them now, and there was a branch for each.
    let mut blocks: Vec<OverviewBlock> = vec![(draw_levels, 1), (draw_channels, 1)];
    if app.stats.responses() > 0 {
        blocks.push((draw_status, 1));
    }
    if app.stats.cache_misses > 0 {
        blocks.push((draw_cache, 1));
    }
    // The errors get twice the width: it is a list of sentences, not figures.
    blocks.push((draw_top_errors, 2));

    let total: u32 = blocks.iter().map(|(_, weight)| weight).sum();
    let areas = Layout::horizontal(
        blocks
            .iter()
            .map(|(_, weight)| Constraint::Ratio(*weight, total)),
    )
    .split(bottom);

    for ((draw_block, _), area) in blocks.iter().zip(areas.iter()) {
        draw_block(frame, app, *area);
    }
}

/// Every line Symfony's cache writes is a miss: it logs when it computes an
/// item and stays silent when it serves one. A key at the top of this list on
/// every request is a cache that is not working.
fn draw_cache(frame: &mut Frame, app: &App, area: Rect) {
    // The same order as the summary's, from the same function: the dashboard
    // and the report must not disagree about which key is worst. Sorted here
    // rather than once per tick, as the other blocks of this row are — the
    // table is capped at a couple of thousand keys, which is nothing beside a
    // frame.
    let lines: Vec<Line> = stats::sorted_cache_keys(&app.stats)
        .into_iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|stat| {
            let count = format!("{:>8} ", format_count(stat.misses()));
            // The key yields to the count rather than pushing it out of the
            // frame.
            let room = (area.width as usize).saturating_sub(2 + count.len());
            Line::from(vec![
                Span::styled(count, Style::new().fg(DIM)),
                Span::raw(stats::truncate(&stat.key, room.clamp(3, 24))),
            ])
        })
        .collect();

    // Plain, like the Levels and Channels blocks beside it: a fifth of the
    // row leaves about eighteen characters for a title, and "Cache — 1,561
    // misses" came out cut mid-word. The total is in the summary and the JSON.
    frame.render_widget(Paragraph::new(lines).block(block("Cache misses")), area);
}

/// The HTTP response classes. This is what the logging level does not say: a
/// 500 caught and logged at `info` is one, and a hundred 404s on
/// `/favicon.ico` are not.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let counts = &app.stats.by_status;
    let max = counts.iter().copied().max().unwrap_or(1).max(1);
    let bar_width = area.width.saturating_sub(16).max(1) as usize;

    let lines: Vec<Line> = [
        (0, "1xx", DIM),
        (1, "2xx", Color::Green),
        (2, "3xx", DIM),
        (3, "4xx", Color::Yellow),
        (4, "5xx", Color::LightRed),
    ]
    .iter()
    .filter(|(index, _, _)| counts[*index] > 0)
    .map(|(index, name, colour)| {
        let count = counts[*index];
        let filled = (count as f64 / max as f64 * bar_width as f64).round() as usize;
        Line::from(vec![
            Span::styled(format!("{name:<5}"), Style::new().fg(*colour)),
            Span::styled(format!("{:>8} ", format_count(count)), Style::new().fg(DIM)),
            Span::styled("█".repeat(filled), Style::new().fg(*colour)),
        ])
    })
    .collect();

    let title = match app.stats.rate_5xx() {
        Some(rate) => format!("Status — {:.1} % 5xx", rate * 100.0),
        None => "Status".into(),
    };
    frame.render_widget(Paragraph::new(lines).block(block(title)), area);
}

fn draw_sparkline(frame: &mut Frame, app: &App, area: Rect, errors_only: bool) {
    // One column = one second: we ask for exactly as many buckets as the block
    // has usable columns.
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
            // Rule of three between the counter and the width available.
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
    // Ties broken by name, as everywhere else: without it, two channels level
    // on count would come out in hash-map order.
    channels.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));

    let items: Vec<ListItem> = channels
        .into_iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|(name, stat)| {
            let compte = format!("{:>8} ", format_count(stat.count));
            let erreurs = (stat.errors > 0).then(|| format!("  ({} err)", stat.errors));
            // The name yields to the rest rather than pushing it out of the
            // frame: the error count is what one comes here for, and it is what
            // a too-long name used to make disappear.
            let reste = (area.width as usize)
                .saturating_sub(2 + compte.len() + erreurs.as_ref().map_or(0, String::len));
            let mut spans = vec![
                Span::styled(compte, Style::new().fg(DIM)),
                Span::raw(stats::truncate(name, reste.clamp(3, 18))),
            ];
            if let Some(erreurs) = erreurs {
                spans.push(Span::styled(erreurs, Style::new().fg(Color::LightRed)));
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
// Tab 2 — errors
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
    // The full detail lives in `Stats`: it is not copied on every tick.
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
    if let Some(endpoint) = stat.subject() {
        lines.push(Line::from(vec![
            Span::styled("raised by  ", Style::new().fg(DIM)),
            Span::styled(endpoint, Style::new().fg(ACCENT)),
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
// Tab 3 — endpoints
// ---------------------------------------------------------------------------

fn draw_endpoints(frame: &mut Frame, app: &App, ui: &mut UiState, area: Rect) {
    let timed: usize = app.route_rows.iter().filter(|r| r.timed > 0).count();

    // The commands take the bottom of the tab when the log carries any: a
    // command is the cron job's endpoint, and it belongs beside the routes
    // rather than among them — every figure in the table above is defined
    // over HTTP requests, which a command is not.
    let (area, commands) = match app.command_rows.is_empty() {
        true => (area, None),
        false => {
            // At most eight rows, and never more than half the tab: the
            // routes are what a diagnosis starts from.
            let rows = (app.command_rows.len() as u16 + 3)
                .min(11)
                .min(area.height / 2);
            let [routes, commands] =
                Layout::vertical([Constraint::Min(5), Constraint::Length(rows)]).areas(area);
            (routes, Some(commands))
        }
    };

    if app.route_rows.is_empty() {
        frame.render_widget(no_endpoints_help(), area);
        if let Some(commands) = commands {
            draw_commands(frame, app, ui, commands);
        }
        return;
    }

    let header = Row::new(vec![
        "Endpoint", "Requests", "SQL/req", "HTTP/req", "p50", "p95", "max", "5xx", "Err.",
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
            // An outbound call costs ten to a hundred times an SQL query, so
            // the threshold that colours it sits an order of magnitude lower.
            Cell::from(if row.avg_calls > 0.0 {
                format!("{:.1}", row.avg_calls)
            } else {
                "—".into()
            })
            .style(if row.avg_calls >= 3.0 {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(DIM)
            }),
            Cell::from(p50),
            Cell::from(p95).style(latency_style(row.p95, row.timed)),
            Cell::from(max).style(Style::new().fg(DIM)),
            Cell::from(match row.responses {
                0 => "—".into(),
                _ => format_count(row.status_5xx),
            })
            .style(if row.status_5xx > 0 {
                Style::new().fg(Color::LightRed).bold()
            } else {
                Style::new().fg(DIM)
            }),
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
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(7),
        ],
    )
    .header(header)
    .block(block(title))
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)).bold())
    .highlight_symbol("▌")
    // Always, so that the gutter stays reserved when the cursor is down in
    // the commands: without it every column would shift by one the moment it
    // crossed over.
    .highlight_spacing(HighlightSpacing::Always);

    // One cursor for the two tables, so only one of them shows it.
    ui.routes
        .select((!app.in_commands).then_some(app.route_sel));
    frame.render_stateful_widget(table, area, &mut ui.routes);

    if let Some(commands) = commands {
        draw_commands(frame, app, ui, commands);
    }
}

/// The console commands, beneath the endpoints, sharing the cursor with them.
fn draw_commands(frame: &mut Frame, app: &App, ui: &mut UiState, area: Rect) {
    let header = Row::new(vec![
        "Command",
        "Runs",
        "Failed",
        "SQL/run",
        "HTTP/run",
        "Last code",
        "p95",
        "Last run",
    ])
    .style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows = app.command_rows.iter().map(|row| {
        Row::new(vec![
            Cell::from(row.name.clone()),
            Cell::from(format_count(row.runs)).style(Style::new().fg(DIM)),
            Cell::from(match row.failed {
                0 => "—".into(),
                n => format_count(n),
            })
            .style(if row.failed > 0 {
                Style::new().fg(Color::LightRed).bold()
            } else {
                Style::new().fg(DIM)
            }),
            // The same two figures the endpoints carry: a nightly import
            // running four thousand queries is the N+1 nobody watches.
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
            Cell::from(if row.avg_calls > 0.0 {
                format!("{:.1}", row.avg_calls)
            } else {
                "—".into()
            })
            .style(if row.avg_calls >= 3.0 {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(DIM)
            }),
            // The exit code is the command's status, and zero is the only
            // good one.
            Cell::from(
                row.last_code
                    .map_or_else(|| "—".to_string(), |code| code.to_string()),
            )
            .style(match row.last_code {
                Some(0) => Style::new().fg(Color::Green),
                Some(_) => Style::new().fg(Color::LightRed).bold(),
                None => Style::new().fg(DIM),
            }),
            // A command writes nothing when it starts: without a token tying
            // its lines together there is no duration, and a dash says so.
            Cell::from(match row.timed {
                0 => "—".into(),
                _ => format_ms(row.p95),
            })
            .style(latency_style(row.p95, row.timed)),
            // "Which commands failed, how often, and since when".
            Cell::from(format_time(row.last_seen)).style(Style::new().fg(DIM)),
        ])
    });

    let failing = app.command_rows.iter().filter(|r| r.failed > 0).count();
    let runs = format_count(app.command_rows.iter().map(|r| r.runs).sum());
    let title = match failing {
        0 => format!(
            "Commands — {} distinct, {runs} runs",
            app.command_rows.len()
        ),
        n => format!(
            "Commands — {} distinct, {runs} runs, {n} failing",
            app.command_rows.len()
        ),
    };

    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(block(title))
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)).bold())
    .highlight_symbol("▌")
    // The same reserved gutter as the table above: the two line up, and
    // nothing moves as the cursor crosses between them.
    .highlight_spacing(HighlightSpacing::Always);

    ui.commands
        .select(app.in_commands.then_some(app.command_sel));
    frame.render_stateful_widget(table, area, &mut ui.commands);
}

/// Sorting by p95 only makes sense if something is timed: when nothing is,
/// better to explain how to fix that than to display a table of dashes.
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
// Tab 4 — SQL and N+1 patterns
// ---------------------------------------------------------------------------

fn draw_sql(frame: &mut Frame, app: &App, ui: &mut UiState, area: Rect) {
    if app.nplus1_rows.is_empty() {
        // A followed endpoint with no N+1 at all is not the same thing as
        // detection being broken: the configuration help would mislead.
        match &app.focus {
            Some(endpoint) => frame.render_widget(nothing_for_focus(endpoint, "N+1 pattern"), area),
            None => frame.render_widget(no_nplus1_help(app), area),
        }
        return;
    }

    let [list, detail] = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).areas(area);

    // "Subject" and not "Endpoint": a cron job repeats a query as readily as
    // a route, and more often keeps the habit — nobody opens a profiler on a
    // nightly import.
    let header = Row::new(vec!["Subject", "Worst", "Avg.", "Runs", "SQL query"])
        .style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows = app.nplus1_rows.iter().map(|row| {
        Row::new(vec![
            Cell::from(row.subject.clone()),
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
            "N+1 patterns — {} found — threshold: {} executions within one run",
            app.nplus1_rows.len(),
            app.cli.nplus1
        ),
    };

    let table = Table::new(
        rows,
        [
            // Fixed width for the endpoint: it is the SQL one wants to read in
            // full, so it is the one that absorbs the remaining room. Two `Min`
            // constraints would share it in equal parts.
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
    let Some(pattern) = app.stats.nplus1.get(&row.key) else {
        return;
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("subject   ", Style::new().fg(DIM)),
            Span::styled(pattern.subject.clone(), Style::new().fg(ACCENT)),
        ]),
        Line::from(vec![
            Span::styled("worst     ", Style::new().fg(DIM)),
            Span::styled(
                format!("{} executions", pattern.max_count),
                severity_style(pattern.max_count),
            ),
            Span::styled(
                format!(
                    "   ·   {:.1} on average over {} runs   ·   last {}",
                    pattern.avg_count(),
                    format_count(pattern.requests),
                    format_time(pattern.last_seen)
                ),
                Style::new().fg(DIM),
            ),
        ]),
        Line::from(""),
        Line::styled(pattern.sql.clone(), Style::new().fg(Color::White)),
    ];

    let detail = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block("Repeated query"));
    frame.render_widget(detail, area);
}

/// Three very different situations hide behind "no N+1": nothing to report, no
/// SQL logged, or no token to group the lines by. Conflating them would let the
/// user believe all is well.
/// The screen of a tab emptied by following an endpoint, rather than by an
/// absence of data: the nuance changes what there is to do.
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
        lines.push(Line::from("  within a single request or command run."));
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
// Tab 5 — outbound HTTP calls
// ---------------------------------------------------------------------------

fn draw_outbound(frame: &mut Frame, app: &App, ui: &mut UiState, area: Rect) {
    if app.outbound_rows.is_empty() {
        frame.render_widget(no_outbound_help(), area);
        return;
    }

    let [list, detail] = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).areas(area);

    let header = Row::new(vec![
        "Call",
        "Calls",
        "Worst/req",
        "p50",
        "p95",
        "max",
        "4xx",
        "5xx",
    ])
    .style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows = app.outbound_rows.iter().map(|row| {
        let (p50, p95, max) = if row.timed > 0 {
            (format_ms(row.p50), format_ms(row.p95), format_ms(row.max))
        } else {
            ("—".into(), "—".into(), "—".into())
        };
        Row::new(vec![
            Cell::from(row.shape.clone()),
            Cell::from(format_count(row.calls)).style(Style::new().fg(DIM)),
            // The worst repetition and not the average: one request calling a
            // provider forty times is the thing to find, and an average over
            // every request that called it once would bury it.
            Cell::from(match row.max_per_request {
                0 => "—".into(),
                n => format!("{n} ×"),
            })
            .style(repetition_style(row.max_per_request)),
            Cell::from(p50),
            Cell::from(p95).style(latency_style(row.p95, row.timed)),
            Cell::from(max).style(Style::new().fg(DIM)),
            Cell::from(counter(row.responses, row.status_4xx))
                .style(status_style(row.status_4xx, Color::Yellow)),
            Cell::from(counter(row.responses, row.status_5xx))
                .style(status_style(row.status_5xx, Color::LightRed)),
        ])
    });

    let title = format!(
        "Outbound HTTP calls — {} shapes — {} calls, {} timed",
        app.outbound_rows.len(),
        format_count(app.stats.http_calls),
        format_count(app.stats.http_timed)
    );

    let table = Table::new(
        rows,
        [
            // The shape absorbs what is left: a host and a path are what one
            // reads here, and the figures beside them are narrow.
            Constraint::Min(30),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(6),
        ],
    )
    .header(header)
    .block(block(title))
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)).bold())
    .highlight_symbol("▌");

    ui.outbound.select(Some(app.outbound_sel));
    frame.render_stateful_widget(table, list, &mut ui.outbound);

    draw_outbound_detail(frame, app, detail);
}

fn draw_outbound_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(row) = app.outbound_rows.get(app.outbound_sel) else {
        return;
    };
    let Some(shape) = app.stats.http.get(&row.key) else {
        return;
    };
    let quantiles = shape.quantiles();

    let latency = if shape.timed > 0 {
        format!(
            "p50 {} · p95 {} · p99 {} · max {}   ({} of {} calls timed)",
            format_ms(quantiles.p50),
            format_ms(quantiles.p95),
            format_ms(quantiles.p99),
            format_ms(shape.max_ms),
            format_count(shape.timed),
            format_count(shape.calls)
        )
    } else {
        // Nothing measured is not "instant": say which field is missing.
        "none measured — the lines carry no total_time".to_string()
    };

    let mut lines = vec![
        Line::styled(shape.shape.clone(), Style::new().fg(Color::White)),
        Line::from(vec![
            Span::styled("latency   ", Style::new().fg(DIM)),
            Span::raw(latency),
        ]),
        Line::from(vec![
            Span::styled("answers   ", Style::new().fg(DIM)),
            Span::raw(match shape.responses {
                0 => "no status logged".to_string(),
                responses => format!(
                    "{} carrying a status · {} × 4xx · {} × 5xx",
                    format_count(responses),
                    format_count(shape.status_4xx),
                    format_count(shape.status_5xx)
                ),
            }),
        ]),
    ];

    if shape.requests > 0 {
        lines.push(Line::from(vec![
            Span::styled("per req.  ", Style::new().fg(DIM)),
            Span::styled(
                format!("{} × at worst", shape.max_per_request),
                repetition_style(shape.max_per_request),
            ),
            Span::styled(
                format!(
                    "   ·   {:.1} on average over {} runs   ·   last {}",
                    shape.avg_per_request(),
                    format_count(shape.requests),
                    format_time(shape.last_seen)
                ),
                Style::new().fg(DIM),
            ),
        ]));
    }
    if let Some(endpoint) = &shape.worst_subject {
        lines.push(Line::from(vec![
            Span::styled("worst from", Style::new().fg(DIM)),
            Span::raw(" "),
            Span::styled(endpoint.clone(), Style::new().fg(ACCENT)),
            Span::styled("   (Enter follows it)", Style::new().fg(DIM)),
        ]));
    }

    let detail = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block("Outbound call"));
    frame.render_widget(detail, area);
}

/// A counter whose denominator is zero says nothing rather than zero: no
/// status read is not "no error".
fn counter(denominator: u64, value: u64) -> String {
    match denominator {
        0 => "—".into(),
        _ => format_count(value),
    }
}

fn status_style(count: u64, colour: Color) -> Style {
    if count > 0 {
        Style::new().fg(colour).bold()
    } else {
        Style::new().fg(DIM)
    }
}

/// The same reading as an N+1: twice is a pattern, twenty times is a loop.
fn repetition_style(count: u32) -> Style {
    if count >= 10 {
        Style::new().fg(Color::Red).bold()
    } else if count >= 4 {
        Style::new().fg(Color::LightRed)
    } else if count >= 2 {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(DIM)
    }
}

/// An empty tab must not read as "this application calls nobody": far more
/// often, the channel simply never reaches a file.
fn no_outbound_help() -> Paragraph<'static> {
    let lines = vec![
        Line::from(""),
        Line::styled(
            "  No outbound HTTP call in the logs.",
            Style::new().fg(Color::Yellow).bold(),
        ),
        Line::from(""),
        Line::from("  refrain reads them from the `http_client` channel, the one Symfony's"),
        Line::from("  HttpClient writes on at INFO level:"),
        Line::from(""),
        Line::styled("    # config/packages/monolog.yaml", Style::new().fg(DIM)),
        Line::styled("    monolog:", Style::new().fg(ACCENT)),
        Line::styled("        handlers:", Style::new().fg(ACCENT)),
        Line::styled("            http_client:", Style::new().fg(ACCENT)),
        Line::styled("                type: stream", Style::new().fg(ACCENT)),
        Line::styled(
            "                path: '%kernel.logs_dir%/http_client.log'",
            Style::new().fg(ACCENT),
        ),
        Line::styled("                level: info", Style::new().fg(ACCENT)),
        Line::styled(
            "                channels: [http_client]",
            Style::new().fg(ACCENT),
        ),
        Line::from(""),
        Line::styled(
            "  Durations need `total_time` in the context — see docs/symfony.md.",
            Style::new().fg(DIM),
        ),
    ];
    Paragraph::new(lines).block(block("Outbound"))
}

// ---------------------------------------------------------------------------
// Tab 6 — messages on the bus
// ---------------------------------------------------------------------------

fn draw_messenger(frame: &mut Frame, app: &App, ui: &mut UiState, area: Rect) {
    if app.message_rows.is_empty() {
        frame.render_widget(no_messenger_help(), area);
        return;
    }

    let [list, detail] = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).areas(area);

    let header = Row::new(vec![
        "Message class",
        "Dispatched",
        "Handled",
        "Waiting",
        "Failed",
        "Lag p95",
        "Worst/req",
        "Dispatched from",
    ])
    .style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows = app.message_rows.iter().map(|row| {
        Row::new(vec![
            Cell::from(row.name.clone()),
            Cell::from(format_count(row.dispatched)).style(Style::new().fg(DIM)),
            Cell::from(format_count(row.handled)).style(Style::new().fg(DIM)),
            // The figure the tab exists for: dispatched minus handled. A
            // queue draining normally sits near zero whatever its volume.
            Cell::from(format_count(row.waiting)).style(waiting_style(row.waiting, row.dispatched)),
            Cell::from(match row.failed {
                0 => "—".into(),
                n => format_count(n),
            })
            .style(if row.failed > 0 {
                Style::new().fg(Color::LightRed).bold()
            } else {
                Style::new().fg(DIM)
            }),
            // Blank and not zero when nothing paired a dispatch with its
            // handling: core Symfony logs no id on dispatch, and "0 ms" would
            // read as a queue with no lag at all.
            Cell::from(match row.timed {
                0 => "—".into(),
                _ => format_ms(row.lag_p95),
            })
            .style(latency_style(row.lag_p95, row.timed)),
            Cell::from(match row.max_per_request {
                0 => "—".into(),
                n => format!("{n} ×"),
            })
            .style(repetition_style(row.max_per_request)),
            // The endpoint that dispatched the most of them within a single
            // request: where a loop on the bus is fixed. Empty for a class
            // only ever dispatched by a worker or a command, which belongs to
            // no request at all.
            Cell::from(row.worst_subject.clone().unwrap_or_else(|| "—".into()))
                .style(Style::new().fg(DIM)),
        ])
    });

    let title = format!(
        "Messenger — {} classes — {} dispatched, {} handled",
        app.message_rows.len(),
        format_count(app.stats.messages_dispatched()),
        format_count(app.stats.messages_handled())
    );

    let table = Table::new(
        rows,
        [
            // The class name is short — the namespace lives in the detail —
            // so it is the endpoint column that absorbs what is left.
            Constraint::Length(30),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .block(block(title))
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)).bold())
    .highlight_symbol("▌");

    ui.messages.select(Some(app.message_sel));
    frame.render_stateful_widget(table, list, &mut ui.messages);

    draw_messenger_detail(frame, app, detail);
}

fn draw_messenger_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(row) = app.message_rows.get(app.message_sel) else {
        return;
    };
    let Some(stat) = app.stats.messages.get(&row.key) else {
        return;
    };

    let mut lines = vec![
        Line::styled(stat.class.clone(), Style::new().fg(Color::White)),
        Line::from(vec![
            Span::styled("queue     ", Style::new().fg(DIM)),
            Span::raw(format!(
                "{} dispatched · {} handled · ",
                format_count(stat.dispatched()),
                format_count(stat.handled())
            )),
            Span::styled(
                format!("{} waiting", format_count(stat.waiting())),
                waiting_style(stat.waiting(), stat.dispatched()),
            ),
            Span::styled(
                format!("   ·   last {}", format_time(stat.last_seen)),
                Style::new().fg(DIM),
            ),
        ]),
        Line::from(vec![
            Span::styled("handling  ", Style::new().fg(DIM)),
            Span::raw(format!(
                "{} handler runs · {} retried · {} failed · {} with no handler",
                format_count(stat.runs),
                format_count(stat.retried),
                format_count(stat.failed),
                format_count(stat.no_handler)
            )),
        ]),
        Line::from(vec![
            Span::styled("lag       ", Style::new().fg(DIM)),
            Span::raw(match stat.timed {
                // Not a defect to hide: core Symfony writes no identifier on
                // the dispatch side, so there is nothing to pair.
                0 => "not measured — no identifier pairs a dispatch with its handling".to_string(),
                timed => {
                    let quantiles = stat.quantiles();
                    format!(
                        "p50 {} · p95 {} · max {}   ({} paired)",
                        format_ms(quantiles.p50),
                        format_ms(quantiles.p95),
                        format_ms(stat.max_ms),
                        format_count(timed)
                    )
                }
            }),
        ]),
    ];

    if stat.requests > 0 {
        lines.push(Line::from(vec![
            Span::styled("per req.  ", Style::new().fg(DIM)),
            Span::styled(
                format!("{} × at worst", stat.max_per_request),
                repetition_style(stat.max_per_request),
            ),
            Span::styled(
                format!(
                    "   ·   {:.1} on average over {} runs",
                    stat.avg_per_request(),
                    format_count(stat.requests)
                ),
                Style::new().fg(DIM),
            ),
        ]));
    }
    if let Some(endpoint) = &stat.worst_subject {
        lines.push(Line::from(vec![
            Span::styled("worst from", Style::new().fg(DIM)),
            Span::raw(" "),
            Span::styled(endpoint.clone(), Style::new().fg(ACCENT)),
            Span::styled("   (Enter follows it)", Style::new().fg(DIM)),
        ]));
    }

    let detail = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block("Message class"));
    frame.render_widget(detail, area);
}

/// A queue that is draining sits near zero whatever its volume; one that is
/// not grows without bound. The share, not the count, is what says which.
fn waiting_style(waiting: u64, dispatched: u64) -> Style {
    if waiting == 0 {
        return Style::new().fg(Color::Green);
    }
    match dispatched {
        0 => Style::new().fg(DIM),
        total if waiting * 2 > total => Style::new().fg(Color::LightRed).bold(),
        _ => Style::new().fg(Color::Yellow),
    }
}

/// An empty tab must not read as "nothing is queued": far more often the
/// channel simply never reaches a file.
fn no_messenger_help() -> Paragraph<'static> {
    let lines = vec![
        Line::from(""),
        Line::styled(
            "  No Messenger line in the logs.",
            Style::new().fg(Color::Yellow).bold(),
        ),
        Line::from(""),
        Line::from("  refrain reads them from the `messenger` channel, at INFO level:"),
        Line::from(""),
        Line::styled("    # config/packages/monolog.yaml", Style::new().fg(DIM)),
        Line::styled("    monolog:", Style::new().fg(ACCENT)),
        Line::styled("        handlers:", Style::new().fg(ACCENT)),
        Line::styled("            messenger:", Style::new().fg(ACCENT)),
        Line::styled("                type: stream", Style::new().fg(ACCENT)),
        Line::styled(
            "                path: '%kernel.logs_dir%/messenger.log'",
            Style::new().fg(ACCENT),
        ),
        Line::styled("                level: info", Style::new().fg(ACCENT)),
        Line::styled(
            "                channels: [messenger]",
            Style::new().fg(ACCENT),
        ),
        Line::from(""),
        Line::from("  Hand the worker's log over with the application's: what a worker"),
        Line::from("  handles is only written where the worker runs."),
        Line::from(""),
        Line::styled(
            "  The lag needs an id on dispatch — see docs/symfony.md.",
            Style::new().fg(DIM),
        ),
    ];
    Paragraph::new(lines).block(block("Messenger"))
}

// ---------------------------------------------------------------------------
// Tab 7 — deprecations
// ---------------------------------------------------------------------------

fn draw_deprecations(frame: &mut Frame, app: &App, ui: &mut UiState, area: Rect) {
    if app.deprecation_rows.is_empty() {
        match &app.focus {
            Some(endpoint) => {
                frame.render_widget(nothing_for_focus(endpoint, "deprecation"), area);
            }
            None => frame.render_widget(no_deprecations_help(), area),
        }
        return;
    }

    let [list, detail] = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).areas(area);

    let header = Row::new(vec!["Count", "Last", "Route", "Deprecation"])
        .style(Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows = app.deprecation_rows.iter().map(|row| {
        Row::new(vec![
            Cell::from(format_count(row.count)).style(Style::new().bold()),
            Cell::from(format_time(row.last_seen)).style(Style::new().fg(DIM)),
            Cell::from(stats::truncate(row.endpoint.as_deref().unwrap_or("—"), 24))
                .style(Style::new().fg(ACCENT)),
            Cell::from(row.key.0.clone()),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(25),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(block(match &app.focus {
        Some(endpoint) => format!(
            "Deprecations of {} ({} distinct)",
            endpoint,
            app.deprecation_rows.len()
        ),
        None => format!(
            "Deprecations ({} lines, {} distinct)",
            format_count(app.stats.deprecations_total),
            app.deprecation_rows.len()
        ),
    }))
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)).bold())
    .highlight_symbol("▌");

    ui.deprecations.select(Some(app.deprecation_sel));
    frame.render_stateful_widget(table, list, &mut ui.deprecations);

    draw_deprecation_detail(frame, app, detail);
}

fn draw_deprecation_detail(frame: &mut Frame, app: &App, area: Rect) {
    let Some(row) = app.deprecation_rows.get(app.deprecation_sel) else {
        return;
    };
    let Some(stat) = app.stats.deprecations.get(&row.key) else {
        return;
    };

    let mut lines = vec![Line::from(vec![
        Span::styled("seen ", Style::new().fg(DIM)),
        Span::styled(format_count(stat.count), Style::new().bold()),
        Span::styled(" times  ·  from ", Style::new().fg(DIM)),
        Span::raw(format_time(stat.first_seen)),
        Span::styled(" to ", Style::new().fg(DIM)),
        Span::raw(format_time(stat.last_seen)),
        Span::styled("  ·  channel ", Style::new().fg(DIM)),
        Span::raw(stat.channel.clone()),
    ])];
    // The origin is what tells where to fix it: the deprecated code itself
    // for a `trigger_deprecation()`, with its real line number.
    if let Some(origin) = &stat.origin {
        lines.push(Line::from(vec![
            Span::styled("origin   ", Style::new().fg(DIM)),
            Span::styled(origin.clone(), Style::new().fg(Color::Yellow)),
        ]));
    }
    if let Some(endpoint) = &stat.endpoint {
        lines.push(Line::from(vec![
            Span::styled("last from ", Style::new().fg(DIM)),
            Span::styled(endpoint.clone(), Style::new().fg(ACCENT)),
        ]));
    }
    lines.push(Line::from(""));
    // The table shows the key, identifiers erased; here is the message as
    // it was written, class names and versions included.
    lines.push(Line::from(stat.message.clone()));

    let detail = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block("Latest occurrence"));
    frame.render_widget(detail, area);
}

/// No deprecation read at all. Either the application has none — or, far
/// more often in production, the handler never lets INFO through: say so,
/// or an empty tab reads as a clean bill of health.
fn no_deprecations_help() -> Paragraph<'static> {
    let lines = vec![
        Line::from(""),
        Line::styled("  No deprecation so far.", Style::new().fg(Color::Green)),
        Line::from(""),
        Line::from("  Symfony logs them on the php channel at INFO — or on the"),
        Line::from("  deprecation channel when the Monolog recipe's handler is set."),
        Line::from("  A handler filtering below INFO, or a fingers_crossed one that"),
        Line::from("  never triggers, keeps them out of the file altogether."),
        Line::from(""),
        Line::styled(
            "  See 'Tracking deprecations' in docs/symfony.md.",
            Style::new().fg(DIM),
        ),
    ];
    Paragraph::new(lines).block(block("Deprecations"))
}

// ---------------------------------------------------------------------------
// Tab 8 — stream
// ---------------------------------------------------------------------------

fn draw_stream(frame: &mut Frame, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    let width = area.width.saturating_sub(2) as usize;

    // The buffer is walked from the end towards the start: recent entries are
    // the interesting ones, and it avoids filtering the whole history.
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
        // The cursor shows that the next keystroke goes to the pattern, not to
        // the shortcuts — that is what tells the two modes apart on screen.
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
    // Multi-line entries (stack traces) are reduced to their first line: the
    // full detail stays available in the Errors tab.
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
// Help
// ---------------------------------------------------------------------------

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered(64, 24, area);
    // `Clear` wipes the area before drawing over it, otherwise the tab's
    // content would show through between the characters.
    frame.render_widget(Clear, popup);

    let rows = [
        ("q", "quit"),
        ("Esc", "drop the current filter, otherwise quit"),
        ("Enter", "follow the endpoint of the selected row"),
        (
            "↓ past the end",
            "step into the commands under the endpoints",
        ),
        ("Tab, ← →", "previous / next tab"),
        ("1 … 8", "jump straight to a tab"),
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

/// Centres a fixed-size rectangle inside an area.
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

    fn app_with_data() -> App {
        let mut app = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        let lines = [
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {"route":"app_home","request_uri":"https://x.test/","method":"GET"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.050000+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT 1"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.100000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Boom: "nope" at /var/www/src/X.php line 12 {"exception":"[object] (App\\Exception\\Boom(code: 0): nope at /var/www/src/X.php:12)"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.110000+02:00] php.INFO: User Deprecated: Since symfony/http-foundation 6.2: Calling "Symfony\Component\HttpFoundation\Request::getContentType()" is deprecated, use "getContentTypeFormat()" instead. {"exception":"[object] (ErrorException(code: 0): User Deprecated: Since symfony/http-foundation 6.2: Calling \"Symfony\\Component\\HttpFoundation\\Request::getContentType()\" is deprecated, use \"getContentTypeFormat()\" instead. at /var/www/vendor/symfony/http-foundation/Request.php:1290)"} {"token":"aaa"}"#,
            r#"[2026-09-09T10:00:00.120000+02:00] request.INFO: Request finished {"route":"app_home","method":"GET","status":500,"duration_ms":120.0} {"token":"aaa"}"#,
        ];
        for line in lines {
            app.stats.ingest(0, parse_line(line).expect("line valide"));
        }
        // A blatant N+1, so the SQL tab has something to show.
        let sql = r#"[2026-09-09T10:00:00.060000+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT t0.id FROM address t0 WHERE t0.customer_id = ?","params":{"1":1}} {"token":"aaa"}"#;
        for _ in 0..14 {
            app.stats
                .ingest(0, parse_line(sql).expect("line SQL valide"));
        }
        // A nightly command that failed: it shares no token with the request
        // — it is another process — and must not land among the endpoints.
        for line in [
            r#"[2026-09-09T10:00:00.000000+02:00] app.INFO: Starting app:import {"batch":500} {"token":"cmd"}"#,
            r#"[2026-09-09T10:00:02.000000+02:00] console.DEBUG: Command "app:import --env=prod" exited with code "1" {"command":"app:import --env=prod","code":1} {"token":"cmd"}"#,
        ] {
            app.stats
                .ingest(0, parse_line(line).expect("ligne console valide"));
        }

        // A cache item computed rather than served: every one of these is a
        // miss, and this one is computed on the only request there is.
        let cache = r#"[2026-09-09T10:00:00.040000+02:00] cache.INFO: Lock acquired, now computing item "nav_menu" {"key":"nav_menu"} {"token":"aaa"}"#;
        app.stats
            .ingest(0, parse_line(cache).expect("ligne cache valide"));

        // A message dispatched and never handled: the queue that is not
        // draining. Both vocabularies, as a real application writes them.
        for i in 0..4 {
            for line in [
                format!(
                    r#"[2026-09-09T10:00:00.080000+02:00] messenger_audit.INFO: [msg{i}] Sent App\Message\IndexEntityMessage {{"id":"msg{i}","class":"App\\Message\\IndexEntityMessage"}} {{"token":"aaa"}}"#
                ),
                r#"[2026-09-09T10:00:00.080000+02:00] messenger.INFO: Sending message App\Message\IndexEntityMessage with async sender using X {"class":"App\\Message\\IndexEntityMessage"} {"token":"aaa"}"#.to_string(),
            ] {
                app.stats
                    .ingest(0, parse_line(&line).expect("ligne messenger valide"));
            }
        }
        let handled = r#"[2026-09-09T10:00:01.080000+02:00] messenger_audit.INFO: [msg0] Received App\Message\IndexEntityMessage [] []"#;
        app.stats
            .ingest(0, parse_line(handled).expect("ligne messenger valide"));

        // And the same loop on a third party, API key in the URL and all.
        for i in 0..3 {
            let call = format!(
                r#"[2026-09-09T10:00:00.070000+02:00] http_client.INFO: Response: "200 https://api.example.com/v1/geocode?id={i}&key=sk_live_9f3c2a" 0.310000 seconds {{"http_method":"GET","http_code":200,"total_time":0.31}} {{"token":"aaa"}}"#
            );
            app.stats
                .ingest(0, parse_line(&call).expect("ligne http_client valide"));
        }
        app.stats.finalize();
        // The Tick builds the sorted tables the display consumes.
        app.on_event(Event::Tick);
        app
    }

    /// Draws into a virtual terminal and returns the raw text of the screen.
    fn render(app: &App, width: u16, height: u16) -> String {
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
    fn the_http_status_shows_when_it_is_logged() {
        let mut app = app_with_data();

        app.tab = Tab::Endpoints;
        let view = render(&app, 140, 40);
        assert!(view.contains("5xx"), "the column must be there: {view}");

        app.tab = Tab::Overview;
        assert!(
            render(&app, 140, 40).contains("Status"),
            "and so must the block"
        );

        // Monolog writes no status of its own: without one, the block
        // disappears instead of taking a quarter of the row for nothing.
        let mut muet = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        let line = r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {"route":"app_home"} []"#;
        muet.stats.ingest(0, parse_line(line).expect("line valide"));
        muet.on_event(Event::Tick);
        assert!(!render(&muet, 140, 40).contains("Status"));
    }

    #[test]
    fn the_banner_announces_a_saturated_table() {
        // The ceiling itself is exercised in `stats`; here we check that it
        // shows. A table that has stopped detailing makes the display partial,
        // and a missing route would otherwise read as a route with no traffic.
        let mut app = app_with_data();
        assert!(!render(&app, 140, 40).contains("capped"));

        app.stats.capped.routes = true;
        assert!(render(&app, 140, 40).contains("capped: routes"));
    }

    #[test]
    fn every_tab_shows_what_is_expected() {
        let mut app = app_with_data();

        app.tab = Tab::Overview;
        let view = render(&app, 140, 40);
        assert!(view.contains("refrain"), "the banner must be there");
        assert!(view.contains("doctrine"), "the channels must appear");
        assert!(
            view.contains("Boom"),
            "the error must surface in the top list"
        );
        assert!(
            view.contains("1 deprecations"),
            "logged at INFO, the banner is the only thing betraying them: {view}"
        );

        app.tab = Tab::Errors;
        let view = render(&app, 140, 40);
        assert!(view.contains("CRITICAL"));
        // The exception has no route context: refrain attaches it to
        // "app_home" through the token shared with the "Matched route" line.
        assert!(view.contains("app_home"), "error attached to its endpoint");

        app.tab = Tab::Endpoints;
        let view = render(&app, 140, 40);
        assert!(view.contains("app_home"));
        assert!(view.contains("120 ms"), "the measured duration must show");
        assert!(
            view.contains("HTTP/req"),
            "and what the request cost elsewhere: {view}"
        );
        // The commands sit below the routes, not among them.
        assert!(view.contains("Commands —"), "the second table: {view}");
        assert!(view.contains("app:import"), "without its arguments: {view}");
        assert!(view.contains("2.00 s"), "and its duration: {view}");
        assert!(
            !app.route_rows.iter().any(|r| r.name.contains("app:import")),
            "a command is not an endpoint"
        );

        app.tab = Tab::Sql;
        let view = render(&app, 140, 40);
        assert!(view.contains("14 ×"), "the worst repetition must show");
        assert!(
            view.contains("FROM address"),
            "the offending query must show"
        );

        app.tab = Tab::Outbound;
        let view = render(&app, 140, 40);
        assert!(
            view.contains("GET api.example.com/v1/geocode"),
            "the call shape must show: {view}"
        );
        assert!(
            !view.contains("sk_live"),
            "and the API key must never reach the screen: {view}"
        );
        assert!(view.contains("310 ms"), "the measured latency: {view}");
        assert!(
            view.contains("3 ×"),
            "three calls within one request: {view}"
        );
        assert!(
            view.contains("app_home"),
            "and the endpoint that made them: {view}"
        );

        app.tab = Tab::Messenger;
        let view = render(&app, 140, 40);
        assert!(
            view.contains("IndexEntityMessage"),
            "the class, without its namespace: {view}"
        );
        assert!(
            view.contains("App\\Message\\IndexEntityMessage"),
            "and with it, in the detail: {view}"
        );
        // Four dispatches, each written twice over by two vocabularies, one
        // of them handled: three are waiting, not seven.
        assert!(
            view.contains("4 dispatched · 1 handled · 3 waiting"),
            "the two vocabularies must count as one: {view}"
        );
        assert!(view.contains("app_home"), "and who dispatches them: {view}");

        app.tab = Tab::Deprecations;
        let view = render(&app, 140, 40);
        assert!(
            view.contains("http-foundation #.#: Calling"),
            "the folded key in the table: {view}"
        );
        // The deprecation names no route: the token attaches it to app_home.
        assert!(view.contains("app_home"), "attached to its route: {view}");
        assert!(
            view.contains("Request.php:1290"),
            "and the detail gives the origin with its real line: {view}"
        );
        assert!(
            view.contains("getContentType"),
            "and the message as written, identifiers included: {view}"
        );

        app.tab = Tab::Stream;
        let view = render(&app, 140, 40);
        assert!(view.contains("Matched route"));
    }

    #[test]
    fn the_commands_table_appears_only_where_commands_ran() {
        // Most applications hand refrain a file with no console line in it,
        // and an empty table would take a third of the tab to say nothing.
        let mut app = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        let line = r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {"route":"app_home","duration_ms":12} []"#;
        app.stats.ingest(0, parse_line(line).expect("line valide"));
        app.on_event(Event::Tick);
        app.tab = Tab::Endpoints;
        let view = render(&app, 140, 40);
        assert!(view.contains("app_home"), "{view}");
        assert!(!view.contains("Commands —"), "{view}");
    }

    #[test]
    fn the_cache_block_appears_only_where_there_is_a_cache() {
        // Not every application has one, and an empty frame would take a
        // fifth of the overview row to say nothing.
        let mut app = app_with_data();
        app.tab = Tab::Overview;
        let view = render(&app, 140, 40);
        assert!(view.contains("Cache"), "the block must be there: {view}");
        assert!(view.contains("nav_menu"), "with the key: {view}");

        let mut without = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        let line = r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {"route":"app_home"} []"#;
        without
            .stats
            .ingest(0, parse_line(line).expect("line valide"));
        without.on_event(Event::Tick);
        assert!(!render(&without, 140, 40).contains("Cache"));
    }

    #[test]
    fn with_no_message_the_tab_says_where_they_would_come_from() {
        // An empty tab must not read as "nothing is queued": far more often
        // the channel simply never reaches a file.
        let mut app = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        app.on_event(Event::Tick);
        app.tab = Tab::Messenger;
        let view = render(&app, 140, 40);
        assert!(view.contains("No Messenger line"), "{view}");
        assert!(view.contains("messenger"), "{view}");
    }

    #[test]
    fn with_no_outbound_call_the_tab_says_where_they_would_come_from() {
        // An empty tab must not read as "this application calls nobody": far
        // more often the channel simply never reaches a file.
        let mut app = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        app.on_event(Event::Tick);
        app.tab = Tab::Outbound;
        let view = render(&app, 140, 40);
        assert!(view.contains("No outbound HTTP call"), "{view}");
        assert!(view.contains("http_client"), "{view}");
    }

    #[test]
    fn with_no_deprecation_the_tab_says_where_they_would_come_from() {
        // An empty tab must not read as a clean bill of health: in production
        // the handler usually never lets INFO through.
        let mut app = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        app.on_event(Event::Tick);
        app.tab = Tab::Deprecations;
        let view = render(&app, 140, 40);
        assert!(view.contains("No deprecation so far"), "{view}");
        assert!(view.contains("php channel at INFO"), "{view}");
    }

    #[test]
    fn le_filtre_de_niveau_du_flux_fonctionne() {
        let mut app = app_with_data();
        app.tab = Tab::Stream;

        assert!(render(&app, 140, 40).contains("Executing statement"));
        app.min_level = Level::Error;
        let view = render(&app, 140, 40);
        assert!(
            !view.contains("Executing statement"),
            "DEBUG must be filtered out"
        );
        assert!(view.contains("Boom"), "CRITICAL must stay");
    }

    fn key_press(app: &mut App, code: KeyCode) {
        app.on_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    #[test]
    fn la_recherche_du_flux_filtre_sans_quitter() {
        let mut app = app_with_data();
        app.tab = Tab::Overview;
        let read = app.stats.total;

        key_press(&mut app, KeyCode::Char('/'));
        assert!(app.searching, "\"/\" opens the input");
        assert!(app.tab == Tab::Stream, "and takes us to the stream");

        // A letter of the pattern must not trigger its shortcut: "q" would
        // quit, "r" would reset the counters.
        key_press(&mut app, KeyCode::Char('q'));
        key_press(&mut app, KeyCode::Char('r'));
        assert!(!app.should_quit, "a \"q\" that was typed does not quit");
        assert_eq!(
            app.stats.total, read,
            "an \"r\" that was typed does not reset"
        );
        key_press(&mut app, KeyCode::Backspace);
        key_press(&mut app, KeyCode::Backspace);
        assert!(app.search.is_empty());

        // Case does not count: the "doctrine" channel answers to "DoCtRiNe".
        for c in "DoCtRiNe".chars() {
            key_press(&mut app, KeyCode::Char(c));
        }
        let view = render(&app, 140, 40);
        assert!(
            view.contains("Executing statement"),
            "the channel must answer"
        );
        assert!(
            !view.contains("Matched route"),
            "the rest of the stream must go"
        );
        assert!(view.contains("DoCtRiNe"), "the typed pattern must show");

        // Enter confirms: the filter stays, the shortcuts come back.
        key_press(&mut app, KeyCode::Enter);
        assert!(!app.searching);
        assert!(render(&app, 140, 40).contains("Executing statement"));

        // Esc clears the pattern and gives the whole stream back.
        key_press(&mut app, KeyCode::Char('/'));
        key_press(&mut app, KeyCode::Esc);
        assert!(app.search.is_empty());
        assert!(!app.should_quit, "Esc while typing does not quit");
        assert!(render(&app, 140, 40).contains("Matched route"));
    }

    #[test]
    fn la_recherche_porte_aussi_sur_l_endpoint_rattache() {
        let mut app = app_with_data();
        app.tab = Tab::Stream;
        app.search = "app_home".to_string();
        let view = render(&app, 140, 40);
        // Neither of these two lines contains "app_home" in its message. The
        // first carries it in its route context; the second, a Doctrine SQL
        // query, names nothing at all — it is the shared token that attaches
        // it, and the stream remembers.
        assert!(view.contains("Request finished"));
        assert!(view.contains("Executing statement"));
        // A pattern matching nothing does empty the stream.
        app.search = "app_checkout".to_string();
        assert!(!render(&app, 140, 40).contains("Request finished"));
    }

    /// Two endpoints, each with its SQL query, its error and its N+1: enough
    /// to check that following one really sets the other aside.
    fn app_deux_endpoints() -> App {
        let mut app = App::new(Cli::parse_from(["refrain", "prod.log"]), 1);
        let mut lines = vec![
            r#"[2026-09-09T10:00:00.000000+02:00] request.INFO: Matched route "app_home". {"route":"app_home"} {"token":"aaa"}"#.to_string(),
            r#"[2026-09-09T10:00:00.010000+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT 1 FROM home"} {"token":"aaa"}"#.to_string(),
            // A valid JSON context, with `method` in it as a real Symfony
            // line carries it. Both mattered: the backslashes were
            // unescaped, so the context never parsed at all, and without a
            // method the error's endpoint was stored bare — which is how the
            // follow came to match by accident.
            r#"[2026-09-09T10:00:00.020000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\Broken: "boom" at /var/www/src/H.php line 3 {"exception":"[object] (App\\Exception\\Broken(code: 0): boom at /var/www/src/H.php:3)","method":"GET"} {"token":"aaa"}"#.to_string(),
            r#"[2026-09-09T10:00:01.000000+02:00] request.INFO: Matched route "app_search". {"route":"app_search"} {"token":"bbb"}"#.to_string(),
            r#"[2026-09-09T10:00:01.010000+02:00] doctrine.DEBUG: Executing statement {"sql":"SELECT 2 FROM search"} {"token":"bbb"}"#.to_string(),
            r#"[2026-09-09T10:00:01.020000+02:00] request.CRITICAL: Uncaught PHP Exception App\Exception\SearchFailed: "empty" at /var/www/src/S.php line 7 {"exception":"[object] (App\\Exception\\SearchFailed(code: 0): empty at /var/www/src/S.php:7)","method":"POST"} {"token":"bbb"}"#.to_string(),
        ];
        // One N+1 for each, so the SQL tab has two rows to filter.
        for (token, table) in [("aaa", "address"), ("bbb", "invoice")] {
            for _ in 0..12 {
                lines.push(format!(
                    r#"[2026-09-09T10:00:0{}.500000+02:00] doctrine.DEBUG: Executing statement {{"sql":"SELECT t0.id FROM {} t0 WHERE t0.x = ?"}} {{"token":"{}"}}"#,
                    if token == "aaa" { 0 } else { 1 },
                    table,
                    token
                ));
            }
        }
        for line in &lines {
            app.stats.ingest(0, parse_line(line).expect("line valide"));
        }
        app.stats.finalize();
        app.on_event(Event::Tick);
        app
    }

    #[test]
    fn suivre_un_endpoint_filtre_erreurs_sql_et_flux() {
        let mut app = app_deux_endpoints();

        // With no follow, both endpoints are there.
        app.tab = Tab::Errors;
        let view = render(&app, 140, 40);
        assert!(view.contains("Broken") && view.contains("SearchFailed"));

        // We follow app_home from the endpoint table.
        app.tab = Tab::Endpoints;
        let position = app
            .route_rows
            .iter()
            .position(|r| r.name == "app_home")
            .expect("app_home must be listed");
        app.route_sel = position;
        key_press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus.as_deref(), Some("app_home"));

        // The endpoint table, itself, keeps everyone: that is where you
        // choose. But the one being followed is marked.
        let view = render(&app, 140, 40);
        assert!(view.contains("app_search"), "the others stay listed");
        assert!(view.contains("▸ app_home"), "the follow is marked");
        assert!(view.contains("following "), "and recalled in the banner");

        app.tab = Tab::Errors;
        let view = render(&app, 140, 40);
        assert!(view.contains("Broken"), "app_home's error stays");
        assert!(!view.contains("SearchFailed"), "app_search's goes away");
        // The verb is shown and not matched on: stored as "GET app_home",
        // the endpoint never equalled the one being followed, and following
        // a route listed none of its errors at all.
        assert!(
            view.contains("GET app_home"),
            "the verb still reads: {view}"
        );

        app.tab = Tab::Sql;
        let view = render(&app, 140, 40);
        assert!(view.contains("address"), "app_home's N+1 stays");
        assert!(!view.contains("invoice"), "app_search's goes away");

        app.tab = Tab::Stream;
        let view = render(&app, 140, 40);
        // The exception names no route in its context: if it is still there,
        // it is indeed the token that attached it to app_home.
        assert!(view.contains("Broken"), "app_home's exception stays");
        assert!(!view.contains("SearchFailed"), "app_search's goes away");
        assert!(!view.contains("app_search"), "ni sa line « Matched route »");

        // Enter on the same row releases the follow.
        app.tab = Tab::Endpoints;
        app.route_sel = position;
        key_press(&mut app, KeyCode::Enter);
        assert!(app.focus.is_none());
        app.tab = Tab::Errors;
        assert!(render(&app, 140, 40).contains("SearchFailed"));
    }

    #[test]
    fn echap_defait_les_filtres_avant_de_quitter() {
        let mut app = app_deux_endpoints();
        app.focus = Some("app_home".to_string());
        app.search = "doctrine".to_string();

        key_press(&mut app, KeyCode::Esc);
        assert!(app.search.is_empty(), "the pattern goes first");
        assert!(app.focus.is_some(), "the follow still holds");
        assert!(!app.should_quit);

        key_press(&mut app, KeyCode::Esc);
        assert!(app.focus.is_none(), "then the follow");
        assert!(!app.should_quit);

        key_press(&mut app, KeyCode::Esc);
        assert!(app.should_quit, "nothing left to undo: we quit");
    }

    #[test]
    fn survit_aux_terminaux_minuscules() {
        // All the layout arithmetic must be saturating: a ridiculously small
        // terminal must not make the program panic.
        let mut app = app_with_data();
        app.show_help = true;
        for (width, height) in [(1, 1), (12, 4), (20, 6), (40, 10), (300, 90)] {
            for tab in Tab::ALL {
                app.tab = tab;
                let _ = render(&app, width, height);
            }
        }
    }
}
