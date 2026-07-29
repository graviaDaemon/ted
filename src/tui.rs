use crate::config::channels::TuiEvent;
use crate::storage::db::RecentFill;
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use std::collections::{HashMap, VecDeque};
use std::io;

const LOG_CAP: usize = 5_000;
const TRADES_CAP: usize = 200;

#[derive(Clone, Copy, PartialEq)]
enum View {
    Dashboard,
    Logs,
}

struct StatusEntry {
    mode: Option<String>,
    realized: f64,
    unrealized: f64,
    equity: f64,
    position: f64,
    open_buys: usize,
    open_sells: usize,
    paused: bool,
    halted: bool,
    fees_paid: f64,
    open_lots: usize,
    trend: Option<String>,
    pnl_7d_pct: Option<f64>,
    bid: Option<f64>,
    active: bool,
}

impl StatusEntry {
    fn new() -> StatusEntry {
        StatusEntry {
            mode: None,
            realized: 0.0,
            unrealized: 0.0,
            equity: 0.0,
            position: 0.0,
            open_buys: 0,
            open_sells: 0,
            paused: false,
            halted: false,
            fees_paid: 0.0,
            open_lots: 0,
            trend: None,
            pnl_7d_pct: None,
            bid: None,
            active: true,
        }
    }

    fn session_pnl(&self) -> f64 {
        self.realized + self.unrealized
    }
}

struct TradeEntry {
    time: String,
    symbol: String,
    is_buy: bool,
    qty: f64,
    price: f64,
    realized: Option<f64>,
}

pub struct Tui {
    terminal: DefaultTerminal,
    view: View,
    input_buf: String,
    cursor_pos: usize,
    prompt: &'static str,
    log_lines: VecDeque<String>,
    /// Lines scrolled up from the bottom of the log; 0 = autoscroll.
    log_scroll: usize,
    statuses: HashMap<String, StatusEntry>,
    /// Newest trade at the front.
    trades: VecDeque<TradeEntry>,
    last_alert: Option<String>,
}

impl Tui {
    pub fn enter() -> io::Result<Tui> {
        let terminal = ratatui::try_init()?;
        let mut tui = Tui {
            terminal,
            view: View::Dashboard,
            input_buf: String::new(),
            cursor_pos: 0,
            prompt: "> ",
            log_lines: VecDeque::new(),
            log_scroll: 0,
            statuses: HashMap::new(),
            trades: VecDeque::new(),
            last_alert: None,
        };
        tui.redraw();
        Ok(tui)
    }

    pub fn exit(&self) {
        ratatui::restore();
    }

    pub fn preload_trades(&mut self, fills: Vec<RecentFill>) {
        for fill in fills {
            let time = DateTime::parse_from_rfc3339(&fill.filled_at)
                .map(|d| d.with_timezone(&Utc).format("%H:%M:%S").to_string())
                .unwrap_or_else(|_| "--:--:--".to_string());
            self.trades.push_back(TradeEntry {
                time,
                symbol: fill.symbol,
                is_buy: fill.direction == "buy",
                qty: fill.quantity,
                price: fill.price,
                realized: fill.realized_pnl,
            });
        }
        self.trades.truncate(TRADES_CAP);
        self.redraw();
    }

    pub fn handle_resize(&mut self) {
        self.redraw();
    }

    pub fn handle_log(&mut self, line: &str) {
        let line = line.trim_end_matches('\n').to_string();
        if line.contains("] [WARN] ")
            || line.contains("] [ERROR] ")
            || line.contains("] [CRITICAL] ")
        {
            self.last_alert = Some(line.clone());
        }
        self.log_lines.push_back(line);
        if self.log_lines.len() > LOG_CAP {
            self.log_lines.pop_front();
        }
        self.redraw();
    }

    pub fn handle_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::Ticker { symbol, bid } => {
                let entry = self.statuses.entry(symbol).or_insert_with(StatusEntry::new);
                entry.bid = Some(bid);
                entry.active = true;
            }
            TuiEvent::Fill {
                symbol,
                is_buy,
                qty,
                price,
                realized_pnl,
                ts,
            } => {
                self.trades.push_front(TradeEntry {
                    time: ts.format("%H:%M:%S").to_string(),
                    symbol,
                    is_buy,
                    qty,
                    price,
                    realized: realized_pnl,
                });
                self.trades.truncate(TRADES_CAP);
            }
            TuiEvent::Status {
                symbol,
                mode,
                realized,
                unrealized,
                equity,
                position,
                open_buys,
                open_sells,
                paused,
                halted,
                fees_paid,
                open_lots,
                trend,
                pnl_7d_pct,
            } => {
                let entry = self.statuses.entry(symbol).or_insert_with(StatusEntry::new);
                entry.mode = Some(mode);
                entry.realized = realized;
                entry.unrealized = unrealized;
                entry.equity = equity;
                entry.position = position;
                entry.open_buys = open_buys;
                entry.open_sells = open_sells;
                entry.paused = paused;
                entry.halted = halted;
                entry.fees_paid = fees_paid;
                entry.open_lots = open_lots;
                entry.trend = trend;
                entry.pnl_7d_pct = pnl_7d_pct;
                entry.active = true;
            }
            TuiEvent::RunnerStopped { symbol } => {
                if let Some(entry) = self.statuses.get_mut(&symbol) {
                    entry.active = false;
                }
            }
        }
        self.redraw();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.kind != KeyEventKind::Press {
            return None;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some("\x04".to_string());
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some("\x04".to_string());
            }
            KeyCode::Tab => {
                self.view = match self.view {
                    View::Dashboard => View::Logs,
                    View::Logs => View::Dashboard,
                };
                self.redraw();
            }
            KeyCode::PageUp => {
                if self.view == View::Logs {
                    let page = self.page_size();
                    self.log_scroll = (self.log_scroll + page).min(self.log_lines.len());
                    self.redraw();
                }
            }
            KeyCode::PageDown => {
                if self.view == View::Logs {
                    let page = self.page_size();
                    self.log_scroll = self.log_scroll.saturating_sub(page);
                    self.redraw();
                }
            }
            KeyCode::Char(c) => {
                self.input_buf.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
                self.redraw();
            }
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.input_buf.remove(self.cursor_pos);
                    self.redraw();
                }
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.redraw();
                }
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input_buf.len() {
                    self.cursor_pos += 1;
                    self.redraw();
                }
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
                self.redraw();
            }
            KeyCode::End => {
                self.cursor_pos = self.input_buf.len();
                self.redraw();
            }
            KeyCode::Enter => {
                let cmd = self.input_buf.clone();
                self.input_buf.clear();
                self.cursor_pos = 0;
                self.redraw();
                return Some(cmd);
            }
            _ => {}
        }
        None
    }

    fn page_size(&self) -> usize {
        self.terminal
            .size()
            .map(|s| s.height.saturating_sub(3).max(1) as usize)
            .unwrap_or(10)
    }

    fn redraw(&mut self) {
        let Tui {
            terminal,
            view,
            input_buf,
            cursor_pos,
            prompt,
            log_lines,
            log_scroll,
            statuses,
            trades,
            last_alert,
        } = self;
        let _ = terminal.draw(|frame| {
            let area = frame.area();
            match view {
                View::Dashboard => {
                    let rows = Layout::vertical([
                        Constraint::Min(3),
                        Constraint::Length(1),
                        Constraint::Length(1),
                    ])
                    .split(area);
                    let cols = Layout::horizontal([
                        Constraint::Percentage(40),
                        Constraint::Percentage(60),
                    ])
                    .split(rows[0]);
                    render_pnl_panel(frame, cols[0], statuses);
                    render_trades_panel(frame, cols[1], trades);
                    render_notice(frame, rows[1], last_alert.as_deref());
                    render_input(frame, rows[2], prompt, input_buf, *cursor_pos);
                }
                View::Logs => {
                    let rows =
                        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
                    render_logs(frame, rows[0], log_lines, *log_scroll);
                    render_input(frame, rows[1], prompt, input_buf, *cursor_pos);
                }
            }
        });
    }
}

fn render_pnl_panel(frame: &mut Frame, area: Rect, statuses: &HashMap<String, StatusEntry>) {
    let block = Block::bordered().title(" PnL (session) — Tab: logs ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if statuses.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "no runners — spawn one to see PnL",
                Style::new().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    }

    let mut symbols: Vec<&String> = statuses.keys().collect();
    symbols.sort();
    let sym_width = symbols.iter().map(|s| s.len()).max().unwrap_or(6);
    let max_abs = statuses
        .values()
        .map(|e| e.session_pnl().abs())
        .fold(0.0_f64, f64::max);
    let label_width = statuses
        .values()
        .map(|e| fmt_signed_usd(e.session_pnl()).len())
        .max()
        .unwrap_or(6);
    let bar_budget = (inner.width as usize).saturating_sub(sym_width + label_width + 3);

    let mut lines: Vec<Line> = Vec::new();
    for sym in symbols {
        let entry = &statuses[sym];
        let pnl = entry.session_pnl();
        let color = if pnl >= 0.0 { Color::Green } else { Color::Red };
        let mut style = Style::new().fg(color);
        let mut dim = Style::new().fg(Color::DarkGray);
        if !entry.active {
            style = style.add_modifier(Modifier::DIM);
            dim = dim.add_modifier(Modifier::DIM);
        }

        let bar_len = if max_abs > 0.0 {
            ((pnl.abs() / max_abs) * bar_budget as f64).round() as usize
        } else {
            0
        };
        let bar = if bar_len == 0 {
            "▏".to_string()
        } else {
            "█".repeat(bar_len.min(bar_budget))
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<sym_width$} ", sym), style),
            Span::styled(format!("{:<bar_budget$} ", bar), style),
            Span::styled(fmt_signed_usd(pnl), style),
        ]));

        let mut parts: Vec<String> = vec![
            entry.mode.clone().unwrap_or_else(|| "…".to_string()),
            entry
                .bid
                .map(fmt_price)
                .unwrap_or_else(|| "—".to_string()),
            format!("pos {}", fmt_qty(entry.position)),
            format!("lots {}", entry.open_lots),
            format!("b{}/s{}", entry.open_buys, entry.open_sells),
            format!("eq {}", fmt_price(entry.equity)),
            format!("fees {}", fmt_price(entry.fees_paid)),
            format!(
                "trend {}",
                entry.trend.clone().unwrap_or_else(|| "—".to_string())
            ),
            format!(
                "7d {}",
                entry
                    .pnl_7d_pct
                    .map(|p| format!("{:+.2}%", p))
                    .unwrap_or_else(|| "n/a".to_string())
            ),
        ];
        if entry.halted {
            parts.push("HALTED".to_string());
        } else if entry.paused {
            parts.push("PAUSED".to_string());
        }
        if !entry.active {
            parts.push("stopped".to_string());
        }
        lines.push(Line::styled(format!("  {}", parts.join(" · ")), dim));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_trades_panel(frame: &mut Frame, area: Rect, trades: &VecDeque<TradeEntry>) {
    let block = Block::bordered().title(" Last trades ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if trades.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "no trades yet",
                Style::new().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    }

    let lines: Vec<Line> = trades
        .iter()
        .take(inner.height as usize)
        .map(|t| {
            let side_color = if t.is_buy { Color::Green } else { Color::Red };
            let mut spans = vec![
                Span::styled(format!("{} ", t.time), Style::new().fg(Color::DarkGray)),
                Span::raw(format!("{:<9} ", t.symbol)),
                Span::styled(
                    format!("{:<6} ", if t.is_buy { "BOUGHT" } else { "SOLD" }),
                    Style::new().fg(side_color),
                ),
                Span::raw(format!("{} @ {}", fmt_qty(t.qty), fmt_price(t.price))),
            ];
            if let Some(r) = t.realized {
                let r_color = if r >= 0.0 { Color::Green } else { Color::Red };
                spans.push(Span::styled(
                    format!(" ({})", fmt_signed_usd(r)),
                    Style::new().fg(r_color),
                ));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_notice(frame: &mut Frame, area: Rect, last_alert: Option<&str>) {
    let line = match last_alert {
        Some(alert) => Line::styled(format!(" ⚠ {}", alert), Style::new().fg(Color::Red)),
        None => Line::raw(""),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_logs(frame: &mut Frame, area: Rect, log_lines: &VecDeque<String>, log_scroll: usize) {
    let block = Block::bordered().title(" Logs — Tab: dashboard · PgUp/PgDn: scroll ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    let total = log_lines.len();
    let offset = log_scroll.min(total.saturating_sub(height));
    let start = total.saturating_sub(height + offset);
    let lines: Vec<Line> = log_lines
        .iter()
        .skip(start)
        .take(height)
        .map(|l| Line::raw(l.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_input(frame: &mut Frame, area: Rect, prompt: &str, input_buf: &str, cursor_pos: usize) {
    frame.render_widget(
        Paragraph::new(format!("{}{}", prompt, input_buf)),
        area,
    );
    let x = area.x + ((prompt.len() + cursor_pos) as u16).min(area.width.saturating_sub(1));
    frame.set_cursor_position(Position::new(x, area.y));
}

fn fmt_signed_usd(v: f64) -> String {
    if v < 0.0 {
        format!("-${:.2}", v.abs())
    } else {
        format!("+${:.2}", v)
    }
}

fn fmt_price(p: f64) -> String {
    if p.abs() < 1.0 {
        format!("{:.6}", p)
    } else {
        format!("{:.2}", p)
    }
}

fn fmt_qty(q: f64) -> String {
    let s = format!("{:.6}", q);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_string()
    } else {
        s.to_string()
    }
}
