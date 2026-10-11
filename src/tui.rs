//! Interactive editor for the config, started by running `lafe-backup` without arguments.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::{
    absolute, backup, check_name, current_schedule, existing_archives, mount_of, parse_keep,
    save_config, schedule_summary, set_schedule, Config,
};

#[derive(Clone, Copy)]
enum Row {
    Header(&'static str),
    Blank,
    Dest,
    Name,
    Keep,
    Schedule,
    Source(usize),
    AddSource,
    Exclude(usize),
    AddExclude,
    Run,
    DryRun,
}

impl Row {
    fn selectable(self) -> bool {
        !matches!(self, Row::Header(_) | Row::Blank)
    }
}

#[derive(Clone, Copy)]
enum Field {
    Dest,
    Name,
    Keep,
    Schedule,
    Source,
    Exclude,
}

impl Field {
    fn prompt(self) -> &'static str {
        match self {
            Field::Dest => "Cílová složka",
            Field::Name => "Jméno archivu",
            Field::Keep => "Kolik záloh ponechat (0 = vše)",
            Field::Schedule => "Výraz (daily 03:15, Mon,Thu 02:30, *-*-01 04:00, off)",
            Field::Source => "Cesta k zálohování",
            Field::Exclude => "Jméno k vynechání",
        }
    }
}

struct Entry {
    name: String,
    is_dir: bool,
}

/// Filesystem picker. Row 0 stands for the current directory itself.
struct Browser {
    for_dest: bool,
    dir: PathBuf,
    entries: Vec<Entry>,
    sel: usize,
    hidden: bool,
    error: Option<String>,
}

impl Browser {
    fn new(for_dest: bool, dir: PathBuf) -> Self {
        let mut b = Browser {
            for_dest,
            dir,
            entries: Vec::new(),
            sel: 0,
            hidden: false,
            error: None,
        };
        b.load();
        b
    }

    fn load(&mut self) {
        self.entries.clear();
        self.error = None;
        match fs::read_dir(&self.dir) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_dir = entry.path().is_dir();
                    if (name.starts_with('.') && !self.hidden) || (self.for_dest && !is_dir) {
                        continue;
                    }
                    self.entries.push(Entry { name, is_dir });
                }
                self.entries
                    .sort_by(|a, b| (!a.is_dir, &a.name).cmp(&(!b.is_dir, &b.name)));
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.sel = self.sel.min(self.entries.len());
    }

    fn enter(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.sel = 0;
        self.load();
    }

    fn parent(&mut self) {
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else {
            return;
        };
        let came_from = self.dir.file_name().map(|n| n.to_string_lossy().into_owned());
        self.enter(parent);
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| Some(&e.name) == came_from.as_ref())
        {
            self.sel = pos + 1;
        }
    }

    /// The highlighted entry, or `None` on the "this directory" row.
    fn current(&self) -> Option<&Entry> {
        self.sel.checked_sub(1).and_then(|i| self.entries.get(i))
    }

    fn highlighted_path(&self) -> PathBuf {
        match self.current() {
            Some(e) => self.dir.join(&e.name),
            None => self.dir.clone(),
        }
    }
}

/// Weekday as systemd spells it and as it reads after "týdně".
const DAYS: [(&str, &str); 7] = [
    ("Mon", "v pondělí"),
    ("Tue", "v úterý"),
    ("Wed", "ve středu"),
    ("Thu", "ve čtvrtek"),
    ("Fri", "v pátek"),
    ("Sat", "v sobotu"),
    ("Sun", "v neděli"),
];

/// The schedules the picker can express.
#[derive(Clone, Copy)]
enum Sched {
    Hours(u32),
    Daily { hour: u32, minute: u32 },
    Weekly { day: usize, hour: u32, minute: u32 },
}

impl Sched {
    /// Reads back a calendar expression written by `set_schedule`.
    fn parse(expr: &str) -> Option<Sched> {
        let expr = expr.strip_prefix("*-*-* ").unwrap_or(expr);
        if expr == "hourly" {
            return Some(Sched::Hours(1));
        }
        if let Some(n) = expr.strip_prefix("0/").and_then(|r| r.strip_suffix(":00:00")) {
            return n.parse().ok().filter(|n| (1..24).contains(n)).map(Sched::Hours);
        }
        let (day, time) = match expr.split_once(' ') {
            Some((d, time)) => (Some(DAYS.iter().position(|(name, _)| *name == d)?), time),
            None => (None, expr),
        };
        let mut parts = time.split(':');
        let hour = parts.next()?.parse().ok().filter(|h| *h < 24)?;
        let minute = parts.next()?.parse().ok().filter(|m| *m < 60)?;
        if !matches!((parts.next(), parts.next()), (None | Some("00"), None)) {
            return None;
        }
        Some(match day {
            Some(day) => Sched::Weekly { day, hour, minute },
            None => Sched::Daily { hour, minute },
        })
    }

    fn label(self) -> String {
        match self {
            Sched::Hours(1) => "každou hodinu".into(),
            Sched::Hours(n @ 2..=4) => format!("každé {n} hodiny"),
            Sched::Hours(n) => format!("každých {n} hodin"),
            Sched::Daily { hour, minute } => format!("denně v {hour:02}:{minute:02}"),
            Sched::Weekly { day, hour, minute } => {
                format!("týdně {} v {hour:02}:{minute:02}", DAYS[day].1)
            }
        }
    }

    /// What `set_schedule` accepts.
    fn command(self) -> String {
        match self {
            Sched::Hours(1) => "hourly".into(),
            Sched::Hours(n) => format!("{n}h"),
            Sched::Daily { hour, minute } => format!("{hour:02}:{minute:02}"),
            Sched::Weekly { day, hour, minute } => {
                format!("{} {hour:02}:{minute:02}", DAYS[day].0)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Choice {
    Off,
    Hours(u32),
    Daily,
    Weekly,
    Custom,
}

const CHOICES: [Choice; 11] = [
    Choice::Off,
    Choice::Hours(1),
    Choice::Hours(2),
    Choice::Hours(3),
    Choice::Hours(4),
    Choice::Hours(6),
    Choice::Hours(8),
    Choice::Hours(12),
    Choice::Daily,
    Choice::Weekly,
    Choice::Custom,
];

/// Schedule picker; the time and weekday apply to the daily and weekly rows.
struct Picker {
    sel: usize,
    day: usize,
    hour: u32,
    minute: u32,
}

impl Picker {
    /// Starts on the row matching the installed timer.
    fn new() -> Self {
        let mut p = Picker {
            sel: 0,
            day: 0,
            hour: 3,
            minute: 0,
        };
        let Some(expr) = current_schedule() else {
            return p;
        };
        let pos = |want: fn(&Choice) -> bool| CHOICES.iter().position(want);
        p.sel = match Sched::parse(&expr) {
            Some(Sched::Hours(n)) => CHOICES
                .iter()
                .position(|c| matches!(c, Choice::Hours(m) if *m == n)),
            Some(Sched::Daily { hour, minute }) => {
                (p.hour, p.minute) = (hour, minute);
                pos(|c| matches!(c, Choice::Daily))
            }
            Some(Sched::Weekly { day, hour, minute }) => {
                (p.day, p.hour, p.minute) = (day, hour, minute);
                pos(|c| matches!(c, Choice::Weekly))
            }
            None => None,
        }
        .unwrap_or(CHOICES.len() - 1);
        p
    }

    fn sched(&self, choice: Choice) -> Option<Sched> {
        let (day, hour, minute) = (self.day, self.hour, self.minute);
        match choice {
            Choice::Hours(n) => Some(Sched::Hours(n)),
            Choice::Daily => Some(Sched::Daily { hour, minute }),
            Choice::Weekly => Some(Sched::Weekly { day, hour, minute }),
            Choice::Off | Choice::Custom => None,
        }
    }
}

/// The installed schedule in words where the picker understands it.
fn schedule_label() -> String {
    let summary = schedule_summary();
    Sched::parse(&summary).map_or(summary, Sched::label)
}

enum Mode {
    Main,
    Input { field: Field, buf: String },
    Browse(Browser),
    Schedule(Picker),
}

enum Action {
    None,
    Quit,
    Backup { dry_run: bool },
}

struct App {
    path: PathBuf,
    cfg: Config,
    sel: usize,
    mode: Mode,
    /// Message and whether it is an error.
    status: Option<(String, bool)>,
    archives: String,
    schedule: String,
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn expand_tilde(input: &str) -> String {
    match input.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            format!("{}{rest}", home().display())
        }
        _ => input.to_string(),
    }
}

fn archive_summary(cfg: &Config) -> String {
    let Some(dest) = &cfg.dest else {
        return String::new();
    };
    match existing_archives(cfg, dest) {
        Ok(found) => match found.last().and_then(|p| p.file_name()) {
            Some(last) => format!(
                "Záloh v cíli: {}, poslední {}",
                found.len(),
                last.to_string_lossy()
            ),
            None => "V cíli zatím není žádná záloha".into(),
        },
        Err(_) => String::new(),
    }
}

impl App {
    fn rows(&self) -> Vec<Row> {
        let mut rows = vec![
            Row::Header("Nastavení"),
            Row::Dest,
            Row::Name,
            Row::Keep,
            Row::Schedule,
            Row::Blank,
            Row::Header("Co zálohovat"),
        ];
        rows.extend((0..self.cfg.sources.len()).map(Row::Source));
        rows.extend([
            Row::AddSource,
            Row::Blank,
            Row::Header("Vynechat (podle jména)"),
        ]);
        rows.extend((0..self.cfg.excludes.len()).map(Row::Exclude));
        rows.extend([
            Row::AddExclude,
            Row::Blank,
            Row::Header("Akce"),
            Row::Run,
            Row::DryRun,
        ]);
        rows
    }

    fn move_sel(&mut self, down: bool) {
        let rows = self.rows();
        let mut i = self.sel;
        loop {
            i = match (down, i) {
                (true, i) if i + 1 < rows.len() => i + 1,
                (false, i) if i > 0 => i - 1,
                _ => return,
            };
            if rows[i].selectable() {
                self.sel = i;
                return;
            }
        }
    }

    /// Rows shift when an item is removed; keep the cursor on something selectable.
    fn fix_sel(&mut self) {
        let rows = self.rows();
        self.sel = self.sel.min(rows.len() - 1);
        if !rows[self.sel].selectable() {
            self.move_sel(false);
        }
    }

    fn ok(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), false));
    }

    fn save(&mut self, msg: impl Into<String>) {
        match save_config(&self.path, &self.cfg) {
            Ok(()) => self.ok(msg),
            Err(e) => self.status = Some((e, true)),
        }
    }

    fn set_dest(&mut self, dest: PathBuf) {
        self.cfg.require_mount = mount_of(&dest);
        let msg = match &self.cfg.require_mount {
            Some(mp) => format!(
                "cíl: {} (záloha poběží jen s připojeným {})",
                dest.display(),
                mp.display()
            ),
            None => format!("cíl: {}", dest.display()),
        };
        self.cfg.dest = Some(dest);
        self.archives = archive_summary(&self.cfg);
        self.save(msg);
    }

    fn toggle_source(&mut self, src: PathBuf) {
        let msg = if let Some(pos) = self.cfg.sources.iter().position(|s| *s == src) {
            self.cfg.sources.remove(pos);
            format!("odebráno: {}", src.display())
        } else {
            let msg = format!("přidáno: {}", src.display());
            self.cfg.sources.push(src);
            msg
        };
        self.save(msg);
    }

    fn apply_input(&mut self, field: Field, value: &str) -> Result<(), String> {
        match field {
            Field::Dest => self.set_dest(absolute(&expand_tilde(value))?),
            Field::Name => {
                check_name(value)?;
                self.cfg.name = value.to_string();
                self.archives = archive_summary(&self.cfg);
                self.save("jméno uloženo");
            }
            Field::Keep => {
                self.cfg.keep = parse_keep(value)?;
                self.save("uloženo");
            }
            Field::Schedule => {
                let msg = set_schedule(value)?;
                self.schedule = schedule_label();
                self.ok(msg);
            }
            Field::Source => {
                let src = absolute(&expand_tilde(value))?;
                fs::symlink_metadata(&src).map_err(|e| format!("{}: {e}", src.display()))?;
                if self.cfg.sources.contains(&src) {
                    return Err(format!("už je v seznamu: {}", src.display()));
                }
                self.toggle_source(src);
            }
            Field::Exclude => {
                if value.contains('/') {
                    return Err("vynechává se podle jména souboru, ne podle cesty".into());
                }
                if !self.cfg.excludes.iter().any(|e| e == value) {
                    self.cfg.excludes.push(value.to_string());
                }
                self.save(format!("vynechává se: {value}"));
            }
        }
        Ok(())
    }

    fn browse_dest(&self) -> Browser {
        let start = self
            .cfg
            .dest
            .as_deref()
            .and_then(|d| d.ancestors().find(|a| a.is_dir()))
            .map_or_else(home, Path::to_path_buf);
        Browser::new(true, start)
    }

    fn on_key(&mut self, key: KeyCode, ctrl: bool) -> Action {
        self.status = None;
        if ctrl {
            // Raw mode swallows SIGINT, so Ctrl+C has to be handled here.
            match (key, &mut self.mode) {
                (KeyCode::Char('c'), _) => return Action::Quit,
                (KeyCode::Char('u'), Mode::Input { buf, .. }) => buf.clear(),
                _ => {}
            }
            return Action::None;
        }
        match std::mem::replace(&mut self.mode, Mode::Main) {
            Mode::Main => return self.key_main(key),
            Mode::Input { field, buf } => self.key_input(key, field, buf),
            Mode::Browse(browser) => self.key_browse(key, browser),
            Mode::Schedule(picker) => self.key_schedule(key, picker),
        }
        Action::None
    }

    fn key_main(&mut self, key: KeyCode) -> Action {
        let row = self.rows()[self.sel];
        let input = |field, buf: String| Mode::Input { field, buf };
        match key {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(false),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(true),
            KeyCode::Char('r') => return Action::Backup { dry_run: false },
            KeyCode::Enter | KeyCode::Char(' ') => match row {
                Row::Dest => self.mode = Mode::Browse(self.browse_dest()),
                Row::Name => self.mode = input(Field::Name, self.cfg.name.clone()),
                Row::Keep => self.mode = input(Field::Keep, self.cfg.keep.to_string()),
                Row::Schedule => self.mode = Mode::Schedule(Picker::new()),
                Row::AddSource => self.mode = Mode::Browse(Browser::new(false, home())),
                Row::AddExclude => self.mode = input(Field::Exclude, String::new()),
                Row::Source(_) | Row::Exclude(_) => self.ok("d = odebrat ze seznamu"),
                Row::Run => return Action::Backup { dry_run: false },
                Row::DryRun => return Action::Backup { dry_run: true },
                Row::Header(_) | Row::Blank => {}
            },
            KeyCode::Char('e') => match row {
                Row::Dest => {
                    let current = self.cfg.dest.as_ref().map(|d| d.display().to_string());
                    self.mode = input(Field::Dest, current.unwrap_or_default());
                }
                Row::AddSource => self.mode = input(Field::Source, String::new()),
                Row::Schedule => {
                    self.mode = input(Field::Schedule, current_schedule().unwrap_or_default())
                }
                _ => {}
            },
            KeyCode::Char('d') | KeyCode::Delete => match row {
                Row::Source(i) => {
                    let src = self.cfg.sources.remove(i);
                    self.save(format!("odebráno: {}", src.display()));
                    self.fix_sel();
                }
                Row::Exclude(i) => {
                    let name = self.cfg.excludes.remove(i);
                    self.save(format!("už se nevynechává: {name}"));
                    self.fix_sel();
                }
                _ => {}
            },
            _ => {}
        }
        Action::None
    }

    fn key_input(&mut self, key: KeyCode, field: Field, mut buf: String) {
        match key {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                let value = buf.trim();
                if !value.is_empty() {
                    if let Err(e) = self.apply_input(field, value) {
                        self.status = Some((e, true));
                    }
                }
                return;
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => buf.push(c),
            _ => {}
        }
        self.mode = Mode::Input { field, buf };
    }

    fn key_browse(&mut self, key: KeyCode, mut b: Browser) {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Up | KeyCode::Char('k') => b.sel = b.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => b.sel = (b.sel + 1).min(b.entries.len()),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => b.parent(),
            KeyCode::Right | KeyCode::Char('l') => {
                if b.current().is_some_and(|e| e.is_dir) {
                    b.enter(b.highlighted_path());
                }
            }
            KeyCode::Enter if b.current().is_some_and(|e| e.is_dir) => {
                b.enter(b.highlighted_path());
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if b.for_dest {
                    self.set_dest(b.highlighted_path());
                    return;
                }
                self.toggle_source(b.highlighted_path());
            }
            KeyCode::Char('.') => {
                b.hidden = !b.hidden;
                b.load();
            }
            KeyCode::Char('e') => {
                let field = if b.for_dest { Field::Dest } else { Field::Source };
                let buf = format!("{}/", b.dir.display()).replace("//", "/");
                self.mode = Mode::Input { field, buf };
                return;
            }
            _ => {}
        }
        self.mode = Mode::Browse(b);
    }

    fn key_schedule(&mut self, key: KeyCode, mut p: Picker) {
        let choice = CHOICES[p.sel];
        let timed = matches!(choice, Choice::Daily | Choice::Weekly);
        match key {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Up | KeyCode::Char('k') => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => p.sel = (p.sel + 1).min(CHOICES.len() - 1),
            KeyCode::Left | KeyCode::Char('h') if timed => p.hour = (p.hour + 23) % 24,
            KeyCode::Right | KeyCode::Char('l') if timed => p.hour = (p.hour + 1) % 24,
            KeyCode::Tab if matches!(choice, Choice::Weekly) => p.day = (p.day + 1) % 7,
            KeyCode::Enter | KeyCode::Char(' ') => {
                let value = match (choice, p.sched(choice)) {
                    (Choice::Custom, _) => {
                        let buf = current_schedule().unwrap_or_default();
                        self.mode = Mode::Input {
                            field: Field::Schedule,
                            buf,
                        };
                        return;
                    }
                    (_, Some(sched)) => sched.command(),
                    (_, None) => "off".into(),
                };
                if let Err(e) = self.apply_input(Field::Schedule, &value) {
                    self.status = Some((e, true));
                }
                return;
            }
            _ => {}
        }
        self.mode = Mode::Schedule(p);
    }

    fn draw_schedule(&self, f: &mut Frame, area: Rect, p: &Picker) {
        let dim = Style::new().fg(Color::DarkGray);
        let items = CHOICES.iter().map(|&choice| {
            let hint = match choice {
                Choice::Daily => "   ←→ hodina",
                Choice::Weekly => "   ←→ hodina · Tab den",
                _ => "",
            };
            let text = match (choice, p.sched(choice)) {
                (_, Some(sched)) => sched.label(),
                (Choice::Custom, _) => "vlastní výraz…".into(),
                _ => "vypnuto".into(),
            };
            Line::from(vec![Span::raw(text), Span::styled(hint, dim)])
        });
        let title = " Jak často zálohovat ".to_string();
        self.render_list(f, area, items.collect(), title, p.sel);
    }

    fn draw(&self, f: &mut Frame) {
        let [body, status, help] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let dim = Style::new().fg(Color::DarkGray);
        let keys = match &self.mode {
            Mode::Browse(b) => {
                self.draw_browser(f, body, b);
                if b.for_dest {
                    "↑↓ pohyb · Enter/→ otevřít · ← zpět · mezerník vybrat jako cíl · . skryté · e napsat cestu · Esc zrušit"
                } else {
                    "↑↓ pohyb · Enter/→ otevřít · ← zpět · mezerník přidat/odebrat · . skryté · e napsat cestu · Esc hotovo"
                }
            }
            Mode::Schedule(p) => {
                self.draw_schedule(f, body, p);
                "↑↓ pohyb · ←→ hodina · Tab den · Enter nastavit · Esc zrušit"
            }
            Mode::Input { .. } => {
                self.draw_main(f, body);
                "Enter potvrdit · Ctrl+U smazat řádek · Esc zrušit"
            }
            Mode::Main => {
                self.draw_main(f, body);
                "↑↓ pohyb · Enter upravit · e napsat cestu ručně · d odebrat · r zálohovat · q konec"
            }
        };

        let status_line = match (&self.mode, &self.status) {
            (Mode::Input { field, buf }, _) => Line::from(vec![
                Span::styled(
                    format!(" {}: ", field.prompt()),
                    Style::new().fg(Color::Yellow),
                ),
                Span::raw(buf.as_str()),
                Span::styled("█", Style::new().fg(Color::Yellow)),
            ]),
            (_, Some((msg, true))) => {
                Line::styled(format!(" {msg}"), Style::new().fg(Color::Red))
            }
            (_, Some((msg, false))) => {
                Line::styled(format!(" {msg}"), Style::new().fg(Color::Green))
            }
            (_, None) => Line::styled(format!(" {}", self.archives), dim),
        };
        f.render_widget(Paragraph::new(status_line), status);
        f.render_widget(Paragraph::new(Line::styled(format!(" {keys}"), dim)), help);
    }

    fn draw_main(&self, f: &mut Frame, area: Rect) {
        let dim = Style::new().fg(Color::DarkGray);
        let field = |label: &str, value: String, note: String| {
            Line::from(vec![
                Span::raw(format!("{label:<10}")),
                Span::styled(value, Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(note, dim),
            ])
        };
        let action = |text: &'static str| Line::styled(text, Style::new().fg(Color::Green));

        let items = self.rows().into_iter().map(|row| match row {
            Row::Header(title) => Line::styled(
                title,
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Row::Blank => Line::raw(""),
            Row::Dest => field(
                "Cíl",
                self.cfg
                    .dest
                    .as_ref()
                    .map_or("(nenastaveno)".into(), |d| d.display().to_string()),
                self.cfg
                    .require_mount
                    .as_ref()
                    .map_or(String::new(), |mp| {
                        format!("  jen s připojeným {}", mp.display())
                    }),
            ),
            Row::Name => field(
                "Jméno",
                self.cfg.name.clone(),
                format!("  -> {}-RRRRMMDD-HHMMSS.tar.gz", self.cfg.name),
            ),
            Row::Keep => field(
                "Ponechat",
                match self.cfg.keep {
                    0 => "vše".into(),
                    n => format!("posledních {n}"),
                },
                String::new(),
            ),
            Row::Schedule => field("Četnost", self.schedule.clone(), String::new()),
            Row::Source(i) => {
                let src = &self.cfg.sources[i];
                let mut line = vec![Span::raw(src.display().to_string())];
                if fs::symlink_metadata(src).is_err() {
                    line.push(Span::styled("  neexistuje!", Style::new().fg(Color::Red)));
                }
                Line::from(line)
            }
            Row::Exclude(i) => Line::raw(self.cfg.excludes[i].as_str()),
            Row::AddSource => action("+ přidat složku nebo soubor…"),
            Row::AddExclude => action("+ přidat jméno…"),
            Row::Run => action("▶ Spustit zálohu teď"),
            Row::DryRun => action("▷ Jen vypsat, co by se zálohovalo"),
        });

        let title = format!(" lafe-backup · {} ", self.path.display());
        self.render_list(f, area, items.collect(), title, self.sel);
    }

    fn draw_browser(&self, f: &mut Frame, area: Rect, b: &Browser) {
        let mark = |path: &Path| {
            if b.for_dest {
                ""
            } else if self.cfg.sources.iter().any(|s| s == path) {
                "[x] "
            } else {
                "[ ] "
            }
        };
        let this_dir = if b.for_dest {
            "● použít tuto složku jako cíl"
        } else {
            "tato složka celá"
        };
        let mut items = vec![Line::styled(
            format!("{}{this_dir}", mark(&b.dir)),
            Style::new().fg(Color::Green),
        )];
        items.extend(b.entries.iter().map(|e| {
            let text = format!("{}{}", mark(&b.dir.join(&e.name)), e.name);
            if e.is_dir {
                Line::styled(format!("{text}/"), Style::new().fg(Color::Blue))
            } else {
                Line::raw(text)
            }
        }));
        if let Some(err) = &b.error {
            items.push(Line::styled(
                format!("nelze číst: {err}"),
                Style::new().fg(Color::Red),
            ));
        }

        let what = if b.for_dest {
            "Kam zálohovat"
        } else {
            "Co zálohovat"
        };
        let title = format!(" {what} · {} ", b.dir.display());
        self.render_list(f, area, items, title, b.sel);
    }

    fn render_list(&self, f: &mut Frame, area: Rect, items: Vec<Line>, title: String, sel: usize) {
        let list = List::new(items)
            .block(Block::bordered().title(title))
            .highlight_symbol("› ")
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default().with_selected(Some(sel));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn event_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<()> {
        loop {
            term.draw(|f| self.draw(f))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match self.on_key(key.code, key.modifiers.contains(KeyModifiers::CONTROL)) {
                Action::None => {}
                Action::Quit => return Ok(()),
                Action::Backup { dry_run } => {
                    // Run on the normal screen so the output stays in the scrollback.
                    ratatui::restore();
                    if let Err(e) = backup(&self.cfg, dry_run) {
                        eprintln!("error: {e}");
                    }
                    println!("\nEnter pro návrat…");
                    io::stdin().read_line(&mut String::new())?;
                    *term = ratatui::init();
                    self.archives = archive_summary(&self.cfg);
                }
            }
        }
    }
}

pub fn run(path: PathBuf, cfg: Config) -> Result<(), String> {
    let mut app = App {
        archives: archive_summary(&cfg),
        schedule: schedule_label(),
        path,
        cfg,
        sel: 1,
        mode: Mode::Main,
        status: None,
    };
    let mut term = ratatui::init();
    let result = app.event_loop(&mut term);
    ratatui::restore();
    result.map_err(|e| format!("terminál: {e}"))
}
