//! Terminal UI showing live grandmaster status.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Row, Table};
use ratatui::Frame;

use crate::gm::Cmd;
use crate::ptp;
use crate::state::{Role, Status};

pub fn run(state: Arc<Mutex<Status>>, cmds: Sender<Cmd>) -> Result<()> {
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, state, &cmds);
    ratatui::restore();
    let _ = cmds.send(Cmd::Quit);
    res
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: Arc<Mutex<Status>>,
    cmds: &Sender<Cmd>,
) -> Result<()> {
    loop {
        let snapshot = state.lock().unwrap().clone();
        terminal.draw(|f| draw(f, &snapshot))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    KeyCode::Char('g') => {
                        let _ = cmds.send(Cmd::ToggleGm);
                    }
                    KeyCode::Char('r') => {
                        let _ = cmds.send(Cmd::RestartHandover);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw(f: &mut Frame, st: &Status) {
    let has_gauge = st.handover_minutes.is_some();
    let mut constraints = vec![Constraint::Length(3)];
    if has_gauge {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Length(11));
    constraints.push(Constraint::Min(5));
    constraints.push(Constraint::Length(1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());
    let mut idx = 0;
    let title_area = rows[idx];
    idx += 1;
    let gauge_area = if has_gauge {
        let a = rows[idx];
        idx += 1;
        Some(a)
    } else {
        None
    };
    let mid_area = rows[idx];
    let bottom_area = rows[idx + 1];
    let footer_area = rows[idx + 2];

    draw_title(f, title_area, st);
    if let Some(area) = gauge_area {
        draw_handover(f, area, st);
    }

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(mid_area);
    draw_clock_info(f, mid[0], st);
    draw_counters(f, mid[1], st);

    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(bottom_area);
    draw_foreign_masters(f, bottom[0], st);
    draw_events(f, bottom[1], st);

    let footer = Line::from(vec![
        Span::styled(" q ", Style::default().bold().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" quit   "),
        Span::styled(" g ", Style::default().bold().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" toggle GM on/off   "),
        Span::styled(" r ", Style::default().bold().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" restart handover countdown"),
    ]);
    f.render_widget(Paragraph::new(footer), footer_area);
}

fn draw_title(f: &mut Frame, area: Rect, st: &Status) {
    let (label, style) = match st.role {
        Role::ActiveGm => (
            "  ACTIVE GRANDMASTER  ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Role::Passive => (
            "  PASSIVE — SWITCH IS GM  ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    };
    let (ts_label, ts_style) = if st.ts_hardware {
        (
            " HW TIMESTAMPS ",
            Style::default().fg(Color::Black).bg(Color::Green),
        )
    } else {
        (
            " ⚠ SW TIMESTAMPS ONLY ",
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        )
    };
    let up = st.started.elapsed().as_secs();
    let title = Line::from(vec![
        Span::styled(label, style),
        Span::raw("  "),
        Span::styled(ts_label, ts_style),
        Span::raw(format!(
            "  gPTP on {}   uptime {:02}:{:02}:{:02}",
            st.iface,
            up / 3600,
            up / 60 % 60,
            up % 60
        )),
    ]);
    f.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::ALL).title(" gptp-gm ")),
        area,
    );
}

fn draw_handover(f: &mut Frame, area: Rect, st: &Status) {
    let total = st.handover_minutes.unwrap_or(0.0) * 60.0;
    let (ratio, label) = match (st.role, st.handover_deadline) {
        (Role::ActiveGm, Some(deadline)) => {
            let remaining = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
            let ratio = if total > 0.0 {
                (remaining / total).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (
                ratio,
                format!(
                    "handing over to switch in {:02}:{:02}",
                    remaining as u64 / 60,
                    remaining as u64 % 60
                ),
            )
        }
        _ => (0.0, "handed over — switch serves as grandmaster".to_string()),
    };
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Handover timer "))
        .gauge_style(Style::default().fg(if ratio > 0.2 { Color::Green } else { Color::Yellow }))
        .ratio(ratio)
        .label(label);
    f.render_widget(gauge, area);
}

fn draw_clock_info(f: &mut Frame, area: Rect, st: &Status) {
    let tai = st.tai_now_secs;
    let (h, m, s) = (tai / 3600 % 24, tai / 60 % 60, tai % 60);
    let ts_line = if st.ts_hardware {
        Line::from(vec![
            Span::styled(format!("{:<22}", "Timestamping"), Style::default().fg(Color::DarkGray)),
            Span::styled(st.ts_desc.clone(), Style::default().fg(Color::Green)),
        ])
    } else {
        Line::from(vec![
            Span::styled(format!("{:<22}", "Timestamping"), Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("⚠ {} — reduced accuracy", st.ts_desc),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ])
    };
    let lines = vec![
        info_line("Clock identity", ptp::fmt_clock_identity(&st.clock_identity)),
        info_line("Domain", st.domain.to_string()),
        info_line(
            "Priority1 / Priority2",
            format!("{} / {}  (lower wins BMCA)", st.priority1, st.priority2),
        ),
        info_line("Clock class", st.clock_class.to_string()),
        info_line("Time source", st.time_source_desc.clone()),
        ts_line,
        info_line("UTC→TAI offset", format!("{} s", st.utc_offset)),
        info_line("TAI time", format!("{h:02}:{m:02}:{s:02}")),
        info_line(
            "Peer link delay",
            st.link_delay_ns
                .map(|d| format!("{:.0} ns", d))
                .unwrap_or_else(|| "no pdelay peer yet".into()),
        ),
    ];
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Clock ")),
        area,
    );
}

fn info_line(key: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<22}"), Style::default().fg(Color::DarkGray)),
        Span::raw(value),
    ])
}

fn draw_counters(f: &mut Frame, area: Rect, st: &Status) {
    let c = &st.counters;
    let rows = vec![
        Row::new(vec!["Announce sent".to_string(), c.announce_tx.to_string()]),
        Row::new(vec!["Sync sent".to_string(), c.sync_tx.to_string()]),
        Row::new(vec!["Follow_Up sent".to_string(), c.follow_up_tx.to_string()]),
        Row::new(vec![
            "Pdelay_Req answered".to_string(),
            format!("{} rx / {} resp", c.pdelay_req_rx, c.pdelay_resp_tx),
        ]),
        Row::new(vec!["Pdelay_Req sent".to_string(), c.pdelay_req_tx.to_string()]),
        Row::new(vec!["Announce received".to_string(), c.announce_rx.to_string()]),
        Row::new(vec![
            "Foreign Sync received".to_string(),
            c.foreign_sync_rx.to_string(),
        ]),
    ];
    let table = Table::new(rows, [Constraint::Length(24), Constraint::Min(10)])
        .block(Block::default().borders(Borders::ALL).title(" Traffic "));
    f.render_widget(table, area);
}

fn draw_foreign_masters(f: &mut Frame, area: Rect, st: &Status) {
    let items: Vec<ListItem> = if st.foreign_masters.is_empty() {
        vec![ListItem::new(Span::styled(
            "none seen in the last 10 s",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        st.foreign_masters
            .iter()
            .map(|fm| {
                let verdict = match (st.role, fm.would_win) {
                    (Role::ActiveGm, true) => {
                        Span::styled(" OUTRANKS US!", Style::default().fg(Color::Red).bold())
                    }
                    (Role::ActiveGm, false) => {
                        Span::styled(" outranked by us", Style::default().fg(Color::Green))
                    }
                    (Role::Passive, _) => {
                        Span::styled(" active GM", Style::default().fg(Color::Yellow))
                    }
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(
                        "{}  p1={} class={} ({} ann)",
                        ptp::fmt_clock_identity(&fm.data.gm_identity),
                        fm.data.priority1,
                        fm.data.clock_class,
                        fm.announces
                    )),
                    verdict,
                ]))
            })
            .collect()
    };
    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Other masters on the network "),
        ),
        area,
    );
}

fn draw_events(f: &mut Frame, area: Rect, st: &Status) {
    let items: Vec<ListItem> = st
        .events
        .iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|e| ListItem::new(e.clone()))
        .collect();
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(" Events ")),
        area,
    );
}
