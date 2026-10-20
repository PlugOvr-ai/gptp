//! Terminal UI showing live grandmaster status.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Row, Sparkline, Table};
use ratatui::Frame;

use crate::config::TimeSync;
use crate::gm::Cmd;
use crate::monitor::fmt_ns;
use crate::ptp;
use crate::state::{Role, Status, SyncMonitor, SyncState};

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
    let monitor = st.sync.as_ref();
    let mut constraints = vec![Constraint::Length(3)];
    if has_gauge {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Length(if monitor.is_some() { 14 } else { 11 }));
    if monitor.is_some() {
        constraints.push(Constraint::Length(5));
    }
    let sync_rows = monitor.map_or(0, time_sync_rows);
    if sync_rows > 0 {
        constraints.push(Constraint::Length(sync_rows + 2));
    }
    constraints.push(Constraint::Min(6));
    constraints.push(Constraint::Length(9));
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
    let spark_area = if monitor.is_some() {
        idx += 1;
        Some(rows[idx])
    } else {
        None
    };
    let time_sync_area = if sync_rows > 0 {
        idx += 1;
        Some(rows[idx])
    } else {
        None
    };
    let nodes_area = rows[idx + 1];
    let bottom_area = rows[idx + 2];
    let footer_area = rows[idx + 3];

    draw_title(f, title_area, st);
    if let Some(area) = gauge_area {
        draw_handover(f, area, st);
    }

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ])
        .split(mid_area);
    match monitor {
        Some(sm) => {
            draw_sync_monitor(f, mid[0], st, sm);
            draw_monitor_counters(f, mid[1], st, sm);
        }
        None => {
            draw_clock_info(f, mid[0], st);
            draw_counters(f, mid[1], st);
        }
    }
    draw_link_health(f, mid[2], st);
    if let (Some(area), Some(sm)) = (spark_area, monitor) {
        draw_residual_history(f, area, sm);
    }
    if let (Some(area), Some(sm)) = (time_sync_area, monitor) {
        draw_time_sync(f, area, sm);
    }
    draw_nodes(f, nodes_area, st);

    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(28),
            Constraint::Percentage(38),
            Constraint::Percentage(34),
        ])
        .split(bottom_area);
    draw_foreign_masters(f, bottom[0], st);
    draw_streams(f, bottom[1], st);
    draw_events(f, bottom[2], st);

    let key =
        |k: &'static str| Span::styled(k, Style::default().bold().fg(Color::Black).bg(Color::Gray));
    let footer = if monitor.is_some() {
        Line::from(vec![key(" q "), Span::raw(" quit")])
    } else {
        Line::from(vec![
            key(" q "),
            Span::raw(" quit   "),
            key(" g "),
            Span::raw(" toggle GM on/off   "),
            key(" r "),
            Span::raw(" restart handover countdown"),
        ])
    };
    f.render_widget(Paragraph::new(footer), footer_area);
}

fn draw_title(f: &mut Frame, area: Rect, st: &Status) {
    let monitor_label;
    let (label, style) = match st.role {
        Role::Monitor => {
            let state = st.sync.as_ref().map_or(SyncState::NoMaster, |s| s.state);
            monitor_label = format!("  SLAVE MONITOR — {}  ", state.label().to_uppercase());
            (
                monitor_label.as_str(),
                sync_state_style(state).add_modifier(Modifier::BOLD),
            )
        }
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
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
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
        _ => (
            0.0,
            "handed over — switch serves as grandmaster".to_string(),
        ),
    };
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Handover timer "),
        )
        .gauge_style(Style::default().fg(if ratio > 0.2 {
            Color::Green
        } else {
            Color::Yellow
        }))
        .ratio(ratio)
        .label(label);
    f.render_widget(gauge, area);
}

fn draw_clock_info(f: &mut Frame, area: Rect, st: &Status) {
    let tai = st.tai_now_secs;
    let (h, m, s) = (tai / 3600 % 24, tai / 60 % 60, tai % 60);
    let ts_line = if st.ts_hardware {
        Line::from(vec![
            Span::styled(
                format!("{:<22}", "Timestamping"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(st.ts_desc.clone(), Style::default().fg(Color::Green)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                format!("{:<22}", "Timestamping"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("⚠ {} — reduced accuracy", st.ts_desc),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ])
    };
    let lines = vec![
        info_line(
            "Clock identity",
            ptp::fmt_clock_identity(&st.clock_identity),
        ),
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

fn sync_state_style(state: SyncState) -> Style {
    let bg = match state {
        SyncState::Locked => Color::Green,
        SyncState::Learning | SyncState::Unstable => Color::Yellow,
        SyncState::NoMaster | SyncState::SyncLost => Color::Red,
    };
    Style::default().fg(Color::Black).bg(bg)
}

fn draw_sync_monitor(f: &mut Frame, area: Rect, st: &Status, sm: &SyncMonitor) {
    let ns = |v: Option<f64>| v.map_or("-".to_string(), fmt_ns);
    let alarm = |cond: bool, text: String| {
        if cond {
            Span::styled(
                text,
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw(text)
        }
    };
    let keyed = |key: &str, value: Span<'static>| {
        Line::from(vec![
            Span::styled(format!("{key:<22}"), Style::default().fg(Color::DarkGray)),
            value,
        ])
    };
    let gm = match &sm.gm {
        Some(g) => format!(
            "{}  p1={} class={} hops={}",
            ptp::fmt_clock_identity(&g.gm_identity),
            g.priority1,
            g.clock_class,
            g.steps_removed
        ),
        None => "none — waiting for Announce".into(),
    };
    let ts_value = if st.ts_hardware {
        Span::styled(st.ts_desc.clone(), Style::default().fg(Color::Green))
    } else {
        Span::styled(
            format!("⚠ {} — reduced accuracy", st.ts_desc),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    let lines = vec![
        keyed(
            "State",
            Span::styled(
                format!(" {} ", sm.state.label()),
                sync_state_style(sm.state),
            ),
        ),
        info_line("Grandmaster", gm),
        info_line(
            "Master − system clock",
            sm.offset_to_system_ns
                .map_or("-".into(), |o| format!("{}  (absolute, coarse)", fmt_ns(o))),
        ),
        info_line(
            "Drift vs local osc.",
            sm.drift_ppb.map_or("-".into(), |d| format!("{d:+.0} ppb")),
        ),
        info_line("Residual (last)", ns(sm.residual_ns)),
        keyed(
            "Jitter RMS / peak",
            alarm(
                sm.state == SyncState::Unstable,
                format!(
                    "{} / {}",
                    ns(sm.rms_ns).trim_start_matches('+'),
                    fmt_ns(sm.peak_ns).trim_start_matches('+')
                ),
            ),
        ),
        keyed(
            "Steps (last)",
            alarm(
                sm.steps > 0,
                format!("{}  ({})", sm.steps, ns(sm.last_step_ns)),
            ),
        ),
        keyed(
            "Spikes/freq/relearn",
            alarm(
                sm.freq_changes + sm.relearns > 0,
                format!("{} / {} / {}", sm.spikes, sm.freq_changes, sm.relearns),
            ),
        ),
        keyed(
            "GM/time-base changes",
            alarm(
                sm.gm_changes + sm.time_base_changes > 0,
                format!("{} / {}", sm.gm_changes, sm.time_base_changes),
            ),
        ),
        keyed("Timestamping", ts_value),
        info_line(
            "Peer link delay",
            match st.link_delay_ns {
                Some(d) => format!("{d:.0} ns"),
                None if sm.listen_only => "not measured (listen-only)".into(),
                None => "no pdelay peer yet".into(),
            },
        ),
        info_line(
            "Residence (corr.)",
            sm.correction_ns.map_or("-".into(), |c| format!("{c} ns")),
        ),
    ];
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Sync monitor "),
        ),
        area,
    );
}

/// Rows of the time sync panel (0 = no panel).
fn time_sync_rows(sm: &SyncMonitor) -> u16 {
    match sm.time_sync {
        None => 0,
        Some(TimeSync::Builtin) => sm.clocks.len().max(1) as u16 + 1,
        Some(TimeSync::Linuxptp) => 2,
    }
}

fn draw_time_sync(f: &mut Frame, area: Rect, sm: &SyncMonitor) {
    let utc = match sm.master_utc_offset {
        Some((o, true)) => format!("master UTC offset {o} s"),
        Some((o, false)) => format!("master UTC offset {o} s — not marked valid"),
        None => "master UTC offset unknown yet".into(),
    };
    let state_style = |state: &str| match state {
        "locked" | "active" => Style::default().fg(Color::Green),
        "holdover" | "waiting" | "activating" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    };
    let mut lines = Vec::new();
    match sm.time_sync {
        Some(TimeSync::Builtin) => {
            lines.push(Line::from(Span::styled(
                format!(
                    "{:<34} {:<10} {:>14} {:>14} {:>6}",
                    "clock", "state", "offset", "frequency", "steps"
                ),
                Style::default().fg(Color::DarkGray),
            )));
            if sm.clocks.is_empty() {
                lines.push(Line::from(Span::styled(
                    "not running — see the event log",
                    Style::default().fg(Color::Red),
                )));
            }
            for c in &sm.clocks {
                let mut spans = vec![
                    Span::raw(format!("{:<34} ", c.name)),
                    Span::styled(format!("{:<10}", c.state), state_style(c.state)),
                    Span::raw(format!(
                        " {:>14} {:>11.0} ppb {:>6}",
                        c.offset_ns.map_or("-".into(), fmt_ns),
                        c.freq_ppb,
                        c.steps
                    )),
                ];
                if let Some(n) = &c.note {
                    spans.push(Span::styled(
                        format!("  {n}"),
                        Style::default().fg(Color::Red),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
        Some(TimeSync::Linuxptp) => {
            let status = sm
                .linuxptp_status
                .clone()
                .unwrap_or_else(|| "checking…".into());
            let healthy = status.split(", ").all(|u| u.ends_with(": active"));
            lines.push(Line::from(vec![
                Span::styled("units  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    status,
                    if healthy {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                    },
                ),
            ]));
            lines.push(Line::from(Span::styled(
                "monitor listen-only beside ptp4l; system clock checked against the master's UTC",
                Style::default().fg(Color::DarkGray),
            )));
        }
        None => {}
    }
    let title = match sm.time_sync {
        Some(TimeSync::Builtin) => format!(" Time sync — built-in servo, {utc} "),
        _ => format!(" Time sync — linuxptp, {utc} "),
    };
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_monitor_counters(f: &mut Frame, area: Rect, st: &Status, sm: &SyncMonitor) {
    let c = &st.counters;
    let warn = |v: u64| {
        if v > 0 {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        }
    };
    let rows = vec![
        Row::new(vec!["Sync received".to_string(), sm.sync_rx.to_string()]),
        Row::new(vec![
            "Follow_Up received".to_string(),
            sm.follow_up_rx.to_string(),
        ]),
        Row::new(vec![
            "Sync interval".to_string(),
            sm.sync_interval_ms
                .map_or("-".into(), |ms| format!("{ms:.1} ms")),
        ]),
        Row::new(vec![
            "Syncs missing (seq)".to_string(),
            sm.missing_syncs.to_string(),
        ])
        .style(warn(sm.missing_syncs)),
        Row::new(vec![
            "Follow_Ups missing".to_string(),
            sm.missing_follow_ups.to_string(),
        ])
        .style(warn(sm.missing_follow_ups)),
        Row::new(vec![
            "Sync timeouts".to_string(),
            sm.sync_timeouts.to_string(),
        ])
        .style(warn(sm.sync_timeouts)),
        Row::new(vec![
            "Jitter bursts".to_string(),
            format!(
                "{}{}{}",
                sm.bursts,
                if sm.in_burst { "  NOW" } else { "" },
                if sm.captures > 0 {
                    format!("  ({} captures)", sm.captures)
                } else {
                    String::new()
                }
            ),
        ])
        .style(warn(sm.in_burst as u64)),
        Row::new(vec![
            "Announce received".to_string(),
            c.announce_rx.to_string(),
        ]),
        Row::new(vec![
            "Other ports' Sync".to_string(),
            c.foreign_sync_rx.to_string(),
        ]),
        Row::new(vec![
            "Pdelay_Req answered".to_string(),
            format!("{} rx / {} resp", c.pdelay_req_rx, c.pdelay_resp_tx),
        ]),
        Row::new(vec![
            "Pdelay_Req sent".to_string(),
            c.pdelay_req_tx.to_string(),
        ]),
    ];
    let table = Table::new(rows, [Constraint::Length(24), Constraint::Min(10)])
        .block(Block::default().borders(Borders::ALL).title(" Traffic "));
    f.render_widget(table, area);
}

fn draw_residual_history(f: &mut Frame, area: Rect, sm: &SyncMonitor) {
    let width = area.width.saturating_sub(2) as usize;
    let data: Vec<u64> = sm
        .history
        .iter()
        .skip(sm.history.len().saturating_sub(width))
        .map(|v| v.round() as u64)
        .collect();
    let max = data.iter().copied().max().unwrap_or(0);
    let style = match sm.state {
        SyncState::Unstable => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::Cyan),
    };
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(format!(
                " |residual| per Sync — scale 0..{} ",
                fmt_ns(max as f64).trim_start_matches('+')
            )))
            .data(&data)
            .style(style),
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
        Row::new(vec![
            "Follow_Up sent".to_string(),
            c.follow_up_tx.to_string(),
        ]),
        Row::new(vec![
            "Pdelay_Req answered".to_string(),
            format!("{} rx / {} resp", c.pdelay_req_rx, c.pdelay_resp_tx),
        ]),
        Row::new(vec![
            "Pdelay_Req sent".to_string(),
            c.pdelay_req_tx.to_string(),
        ]),
        Row::new(vec![
            "Announce received".to_string(),
            c.announce_rx.to_string(),
        ]),
        Row::new(vec![
            "Foreign Sync received".to_string(),
            c.foreign_sync_rx.to_string(),
        ]),
    ];
    let table = Table::new(rows, [Constraint::Length(24), Constraint::Min(10)])
        .block(Block::default().borders(Borders::ALL).title(" Traffic "));
    f.render_widget(table, area);
}

fn fmt_bps(bps: f64) -> String {
    if bps >= 1e6 {
        format!("{:.1} Mb/s", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.0} kb/s", bps / 1e3)
    } else {
        format!("{bps:.0} b/s")
    }
}

fn draw_link_health(f: &mut Frame, area: Rect, st: &Status) {
    let l = &st.link;
    let mut lines = vec![
        info_line("RX", format!("{} ({} pkt/s)", fmt_bps(l.rx_bps), l.rx_pps)),
        info_line("TX", format!("{} ({} pkt/s)", fmt_bps(l.tx_bps), l.tx_pps)),
    ];
    if l.problems.is_empty() {
        lines.push(Line::from(Span::styled(
            "no drop/error counters raised",
            Style::default().fg(Color::Green),
        )));
    } else {
        for c in l.problems.iter().take(6) {
            let style = if c.delta > 0 {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow)
            };
            let delta = if c.delta > 0 {
                format!("  +{}/s", c.delta)
            } else {
                String::new()
            };
            lines.push(Line::from(Span::styled(
                format!("{:<18} {}{delta}", c.name, c.total),
                style,
            )));
        }
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Link health ({}) ", st.iface)),
        ),
        area,
    );
}

fn draw_streams(f: &mut Frame, area: Rect, st: &Status) {
    let block = Block::default().borders(Borders::ALL).title(format!(
        " Streams — sequence-gap loss ({}) ",
        st.streams.len()
    ));
    if st.streams.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "no RTP/AVTP/SOME-IP streams seen",
                Style::default().fg(Color::DarkGray),
            ))
            .block(block),
            area,
        );
        return;
    }
    let header = Row::new(vec!["kind", "from", "to", "rate", "lost", "ooo"])
        .style(Style::default().fg(Color::DarkGray));
    let rows: Vec<Row> = st
        .streams
        .iter()
        .take(area.height.saturating_sub(3) as usize)
        .map(|s| {
            let from = st
                .nodes
                .iter()
                .find(|n| n.mac == s.src_mac)
                .and_then(|n| n.name.as_ref().map(|(name, _)| name.clone()))
                .unwrap_or_else(|| crate::inventory::fmt_mac(&s.src_mac));
            let loss_pct = if s.packets + s.lost > 0 {
                s.lost as f64 / (s.packets + s.lost) as f64 * 100.0
            } else {
                0.0
            };
            let style = if s.lost > 0 {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            Row::new(vec![
                s.kind.to_string(),
                from,
                s.dst.clone(),
                fmt_bps(s.rate_bps),
                format!("{} ({loss_pct:.2}%)", s.lost),
                s.out_of_order.to_string(),
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(18),
            Constraint::Length(20),
            Constraint::Length(11),
            Constraint::Length(14),
            Constraint::Min(4),
        ],
    )
    .header(header)
    .block(block);
    f.render_widget(table, area);
}

fn draw_nodes(f: &mut Frame, area: Rect, st: &Status) {
    let block = Block::default().borders(Borders::ALL).title(format!(
        " Network nodes — {} seen (RTT = network latency, not sync accuracy) ",
        st.nodes.len()
    ));
    if st.nodes.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "no nodes discovered yet — nodes appear when they send any traffic \
                 (use --scan-subnet to probe silent ones)",
                Style::default().fg(Color::DarkGray),
            ))
            .block(block),
            area,
        );
        return;
    }
    let header = Row::new(vec![
        "Name", "IP", "MAC", "load", "RTT", "avg", "jitter", "loss", "seen", "via",
    ])
    .style(Style::default().fg(Color::DarkGray));
    let us = |v: Option<f64>| v.map_or("-".to_string(), |x| format!("{:.0} µs", x));
    let rows: Vec<Row> = st
        .nodes
        .iter()
        .map(|n| {
            let loss = if n.probes_sent > 0 {
                format!("{}/{}", n.probes_lost, n.probes_sent)
            } else {
                "-".to_string()
            };
            let age = n.last_seen.elapsed().as_secs();
            let jitter_warn = n.jitter_us.is_some_and(|j| j > 1000.0);
            let style = if jitter_warn {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            Row::new(vec![
                n.name
                    .as_ref()
                    .map_or("-".to_string(), |(name, _)| name.clone()),
                n.ip.map_or("-".to_string(), |i| i.to_string()),
                crate::inventory::fmt_mac(&n.mac),
                fmt_bps(n.rate_bps),
                us(n.rtt_last_us),
                us(n.rtt_avg_us),
                us(n.jitter_us) + if jitter_warn { " !" } else { "" },
                loss,
                format!("{age}s ago"),
                match n.method {
                    crate::state::ProbeMethod::Icmp => "icmp",
                    crate::state::ProbeMethod::Arp => "arp",
                    crate::state::ProbeMethod::None => "passive",
                }
                .to_string(),
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Length(15),
            Constraint::Length(18),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Min(7),
        ],
    )
    .header(header)
    .block(block);
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
                    (Role::Monitor, true) => {
                        Span::styled(" following", Style::default().fg(Color::Green))
                    }
                    (Role::Monitor, false) => {
                        Span::styled(" standby", Style::default().fg(Color::DarkGray))
                    }
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(
                        "{}  p1={} class={} hops={} ({} ann)",
                        ptp::fmt_clock_identity(&fm.data.gm_identity),
                        fm.data.priority1,
                        fm.data.clock_class,
                        fm.data.steps_removed,
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(st: &Status) -> String {
        let mut term = Terminal::new(TestBackend::new(160, 50)).unwrap();
        term.draw(|f| draw(f, st)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn both_modes_render() {
        let gm = render(&Status::new("eth0".into()));
        assert!(gm.contains("ACTIVE GRANDMASTER"));

        let mut st = Status::new("eth0".into());
        st.role = Role::Monitor;
        let mut sm = SyncMonitor {
            state: SyncState::Locked,
            residual_ns: Some(-42.0),
            drift_ppb: Some(1234.0),
            steps: 1,
            last_step_ns: Some(3e9),
            ..Default::default()
        };
        sm.history.extend((0..200).map(|i| (i % 17) as f64 * 10.0));
        sm.time_sync = Some(TimeSync::Builtin);
        sm.master_utc_offset = Some((37, true));
        sm.clocks = vec![crate::clocksync::ClockView {
            name: "NIC clock eth0 (/dev/ptp0)".into(),
            state: "locked",
            offset_ns: Some(-12.0),
            freq_ppb: -4200.0,
            steps: 1,
            note: None,
        }];
        st.sync = Some(sm);
        let mon = render(&st);
        if std::env::var_os("SHOW_TUI").is_some() {
            println!("{mon}");
        }
        assert!(mon.contains("SLAVE MONITOR — LOCKED"));
        assert!(mon.contains("Sync monitor"));
        assert!(mon.contains("+3.000000 s"));
        assert!(!mon.contains("toggle GM"));
        assert!(mon.contains("Time sync — built-in servo, master UTC offset 37 s"));
        assert!(mon.contains("NIC clock eth0 (/dev/ptp0)"));
    }
}
