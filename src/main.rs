use std::env;
use std::fs::{self, File};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::os::unix::fs::{chown, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::write::GzEncoder;
use flate2::Compression;

mod tui;

const USAGE: &str = "\
lafe-backup - zabalí nastavené složky do .tar.gz v cílové složce a smaže staré zálohy

Použití:
  lafe-backup                      interaktivní nastavení v terminálu
  lafe-backup add <cesta>...       přidat složky/soubory k zálohování
  lafe-backup remove <cesta>...    odebrat je ze zálohování
  lafe-backup dest <složka>        nastavit, kam se archivy ukládají
  lafe-backup exclude <jméno>...   přeskakovat soubory/složky s tímto jménem
  lafe-backup unexclude <jméno>... přestat je přeskakovat
  lafe-backup keep <n>             kolik posledních archivů ponechat (0 = nemazat)
  lafe-backup name <jméno>         prefix archivu -> <jméno>-RRRRMMDD-HHMMSS.tar.gz
  lafe-backup schedule <kdy>       jak často zálohovat: daily 03:00, 6h, 5m, 30s, hourly,
                                   Mon 02:30, weekly, off (nebo systemd OnCalendar)
  lafe-backup show | list          vypsat aktuální nastavení
  lafe-backup run [--dry-run]      provést zálohu (--dry-run jen vypíše, co by se zálohovalo)

Nastavení je v ~/.config/lafe-backup/config (jinou cestu určí LAFE_BACKUP_CONFIG).
Systémové složky: `sudo lafe-backup ...` pracuje se stejným nastavením a příkaz
schedule pak zapne systémový timer, takže zálohy běží jako root.
Symlinky se ukládají jako symlinky (nenásledují se).";

const SERVICE: &str = "lafe-backup.service";
const TIMER: &str = "lafe-backup.timer";
const SYSTEM_UNITS: &str = "/etc/systemd/system";

struct Config {
    sources: Vec<PathBuf>,
    dest: Option<PathBuf>,
    name: String,
    keep: usize,
    excludes: Vec<String>,
    /// Set automatically by `dest` when the destination lives on a mounted filesystem.
    require_mount: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sources: Vec::new(),
            dest: None,
            name: String::from("backup"),
            keep: 14,
            excludes: Vec::new(),
            require_mount: None,
        }
    }
}

#[derive(Default)]
struct Stats {
    files: u64,
    bytes: u64,
    skipped: u64,
}

fn is_root() -> bool {
    fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

/// The user who ran us through sudo.
struct Invoker {
    name: String,
    uid: u32,
    gid: u32,
    home: PathBuf,
}

fn invoker() -> Option<Invoker> {
    if !is_root() {
        return None;
    }
    let uid = env::var("SUDO_UID").ok()?;
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 6 || f[2] != uid {
            return None;
        }
        Some(Invoker {
            name: f[0].to_string(),
            uid: f[2].parse().ok()?,
            gid: f[3].parse().ok()?,
            home: PathBuf::from(f[5]),
        })
    })
}

/// Under sudo this is the invoking user's directory, so both share one config.
fn config_home() -> Result<PathBuf, String> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(inv) = invoker() {
        return Ok(inv.home.join(".config"));
    }
    Ok(PathBuf::from(env::var_os("HOME").ok_or("není nastaveno HOME")?).join(".config"))
}

fn config_path() -> Result<PathBuf, String> {
    if let Some(p) = env::var_os("LAFE_BACKUP_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    Ok(config_home()?.join("lafe-backup").join("config"))
}

fn parse_keep(v: &str) -> Result<usize, String> {
    v.parse().map_err(|_| format!("neplatný počet archivů: {v}"))
}

fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains(['/', '\n']) {
        return Err(format!("neplatné jméno: {name:?}"));
    }
    Ok(())
}

/// The config is a list of `key=value` lines; `source` and `exclude` may repeat.
fn load_config(path: &Path) -> Result<Config, String> {
    let mut cfg = Config::default();
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(cfg),
        Err(e) => return Err(format!("nelze číst {}: {e}", path.display())),
    };
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = |why: &str| format!("{}:{}: {why}", path.display(), i + 1);
        let (key, value) = line.split_once('=').ok_or_else(|| bad("chybí '='"))?;
        match key {
            "source" => cfg.sources.push(PathBuf::from(value)),
            "dest" => cfg.dest = Some(PathBuf::from(value)),
            "name" => cfg.name = value.to_string(),
            "keep" => cfg.keep = parse_keep(value).map_err(|e| bad(&e))?,
            "exclude" => cfg.excludes.push(value.to_string()),
            "require_mount" => cfg.require_mount = Some(PathBuf::from(value)),
            _ => return Err(bad(&format!("neznámý klíč {key:?}"))),
        }
    }
    check_name(&cfg.name)?;
    Ok(cfg)
}

fn save_config(path: &Path, cfg: &Config) -> Result<(), String> {
    let mut text = String::new();
    if let Some(dest) = &cfg.dest {
        text += &format!("dest={}\n", dest.display());
    }
    if let Some(mp) = &cfg.require_mount {
        text += &format!("require_mount={}\n", mp.display());
    }
    text += &format!("name={}\nkeep={}\n", cfg.name, cfg.keep);
    for src in &cfg.sources {
        text += &format!("source={}\n", src.display());
    }
    for ex in &cfg.excludes {
        text += &format!("exclude={ex}\n");
    }
    let write = || -> io::Result<()> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
        if let Some(dir) = dir {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &text)?;
        fs::rename(&tmp, path)?;
        // Written through sudo: keep the config editable by its owner.
        if let Some(inv) = invoker().filter(|inv| path.starts_with(&inv.home)) {
            for p in dir.into_iter().chain([path]) {
                chown(p, Some(inv.uid), Some(inv.gid))?;
            }
        }
        Ok(())
    };
    write().map_err(|e| format!("nelze zapsat {}: {e}", path.display()))
}

/// Absolute path without `.` components or a trailing slash; symlinks are kept.
fn absolute(arg: &str) -> Result<PathBuf, String> {
    if arg.contains('\n') {
        return Err(format!("cesta nesmí obsahovat nový řádek: {arg:?}"));
    }
    let abs = std::path::absolute(arg).map_err(|e| format!("neplatná cesta {arg:?}: {e}"))?;
    Ok(abs.components().collect())
}

fn mountpoints() -> io::Result<Vec<PathBuf>> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    Ok(mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(PathBuf::from)
        .collect())
}

/// Without this check an unmounted storage box would silently fill the local disk.
fn is_mountpoint(path: &Path) -> io::Result<bool> {
    let path = fs::canonicalize(path)?;
    Ok(mountpoints()?.contains(&path))
}

/// The non-root filesystem `path` lives on, if any. `path` need not exist yet.
fn mount_of(path: &Path) -> Option<PathBuf> {
    let existing = path.ancestors().find_map(|a| fs::canonicalize(a).ok())?;
    mountpoints()
        .ok()?
        .into_iter()
        .filter(|mp| existing.starts_with(mp) && mp != Path::new("/"))
        .max_by_key(|mp| mp.as_os_str().len())
}

/// UTC timestamp as YYYYMMDD-HHMMSS.
fn timestamp(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

struct Walker<'a, W: Write> {
    cfg: &'a Config,
    dest: &'a Path,
    /// `None` in dry-run mode.
    tar: Option<tar::Builder<W>>,
    stats: Stats,
}

impl<W: Write> Walker<'_, W> {
    fn skip(&mut self, path: &Path, why: &dyn std::fmt::Display) {
        eprintln!("varování: přeskakuji {}: {why}", path.display());
        self.stats.skipped += 1;
    }

    /// Errors returned from here mean the archive itself is broken; problems
    /// with a single source entry are reported via `skip` instead.
    fn add(&mut self, path: &Path) -> io::Result<()> {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| self.cfg.excludes.iter().any(|e| e == n))
        {
            return Ok(());
        }
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                self.skip(path, &e);
                return Ok(());
            }
        };
        // Stored like GNU tar does it: absolute path without the leading slash.
        let name = path.strip_prefix("/").unwrap_or(path);
        let ftype = meta.file_type();

        if ftype.is_dir() {
            if path == self.dest {
                return Ok(());
            }
            let mut children = match fs::read_dir(path) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .collect::<Vec<_>>(),
                Err(e) => {
                    self.skip(path, &e);
                    return Ok(());
                }
            };
            children.sort();
            if let Some(tar) = &mut self.tar {
                tar.append_path_with_name(path, name)?;
            }
            for child in children {
                self.add(&child)?;
            }
        } else if ftype.is_file() {
            // Opened here so that an unreadable file is skipped before
            // anything is written into the archive.
            let mut file = match File::open(path) {
                Ok(f) => f,
                Err(e) => {
                    self.skip(path, &e);
                    return Ok(());
                }
            };
            match &mut self.tar {
                Some(tar) => tar.append_file(name, &mut file)?,
                None => println!("{}", path.display()),
            }
            self.stats.files += 1;
            self.stats.bytes += meta.len();
        } else if ftype.is_symlink() {
            match &mut self.tar {
                Some(tar) => tar.append_path_with_name(path, name)?,
                None => println!("{}", path.display()),
            }
            self.stats.files += 1;
        } else {
            self.skip(path, &"není soubor, složka ani symlink");
        }
        Ok(())
    }
}

/// Archives of this backup set, oldest first.
fn existing_archives(cfg: &Config, dest: &Path) -> io::Result<Vec<PathBuf>> {
    let prefix = format!("{}-", cfg.name);
    let mut found = Vec::new();
    for entry in fs::read_dir(dest)? {
        let path = entry?.path();
        let is_ours = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(&prefix)?.strip_suffix(".tar.gz"))
            .is_some_and(|ts| {
                ts.len() == 15
                    && ts
                        .bytes()
                        .enumerate()
                        .all(|(i, b)| if i == 8 { b == b'-' } else { b.is_ascii_digit() })
            });
        if is_ours {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

fn prune(cfg: &Config, dest: &Path) -> io::Result<()> {
    if cfg.keep == 0 {
        return Ok(());
    }
    let archives = existing_archives(cfg, dest)?;
    let excess = archives.len().saturating_sub(cfg.keep);
    for old in &archives[..excess] {
        fs::remove_file(old)?;
        println!("smazána stará záloha {}", old.display());
    }
    Ok(())
}

/// The timer is a system unit once root has scheduled it, a user unit otherwise.
fn system_timer() -> bool {
    is_root() || Path::new(SYSTEM_UNITS).join(TIMER).exists()
}

fn unit_dir() -> Result<PathBuf, String> {
    if system_timer() {
        return Ok(PathBuf::from(SYSTEM_UNITS));
    }
    Ok(config_home()?.join("systemd").join("user"))
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let scope: &[&str] = if system_timer() { &[] } else { &["--user"] };
    let out = Command::new("systemctl")
        .args(scope)
        .args(args)
        .output()
        .map_err(|e| format!("nelze spustit systemctl: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "systemctl {} selhalo: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// Turns `6h`, `5m`, `30s` or `daily 03:00` into a systemd calendar expression.
/// Anything else (`hourly`, `Mon 02:30`, `*-*-01 04:00`) already is one.
fn calendar_expr(input: &str) -> Result<String, String> {
    if input.contains('\n') {
        return Err("četnost nesmí obsahovat nový řádek".into());
    }
    let count = |unit: &str| input.strip_suffix(unit).and_then(|n| n.parse::<u32>().ok());
    if let Some(n) = count("s") {
        return match n {
            1..=59 => Ok(format!("*:*:0/{n}")),
            _ => Err("interval v sekundách musí být 1s až 59s (jinak 1m)".into()),
        };
    }
    if let Some(n) = count("min").or_else(|| count("m")) {
        return match n {
            1..=59 => Ok(format!("*:0/{n}:00")),
            _ => Err("interval v minutách musí být 1m až 59m (jinak hourly)".into()),
        };
    }
    if let Some(n) = input.strip_suffix('h').and_then(|n| n.parse::<u32>().ok()) {
        return match n {
            1..=23 => Ok(format!("0/{n}:00:00")),
            _ => Err("interval v hodinách musí být 1h až 23h (jinak daily)".into()),
        };
    }
    Ok(match input.strip_prefix("daily ") {
        Some(time) => time.trim().to_string(),
        None => input.to_string(),
    })
}

/// Validates `expr` with systemd and returns when it would fire next.
fn next_elapse(expr: &str) -> Result<String, String> {
    let out = Command::new("systemd-analyze")
        .args(["calendar", expr])
        .output()
        .map_err(|e| format!("nelze spustit systemd-analyze: {e}"))?;
    if !out.status.success() {
        return Err(format!("neplatná četnost: {expr:?}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("Next elapse: "))
        .unwrap_or("?")
        .to_string())
}

/// The schedule lives in the systemd user timer, which is generated here.
fn set_schedule(input: &str) -> Result<String, String> {
    if !is_root() && system_timer() {
        return Err("zálohy spouští systémový timer: použijte sudo lafe-backup schedule".into());
    }
    let dir = unit_dir()?;
    let timer = dir.join(TIMER);
    let io_err = |p: &Path, e: io::Error| format!("nelze zapsat {}: {e}", p.display());

    // The system timer replaces the invoking user's own one.
    if let Some(inv) = invoker() {
        let own = inv.home.join(".config/systemd/user").join(TIMER);
        if own.exists() {
            let machine = format!("{}@", inv.name);
            let out = Command::new("systemctl")
                .args(["--user", "-M", &machine, "disable", "--now", TIMER])
                .output()
                .map_err(|e| format!("nelze spustit systemctl: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "nelze vypnout timer uživatele {}: {}",
                    inv.name,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            fs::remove_file(&own).map_err(|e| io_err(&own, e))?;
        }
    }

    if input == "off" {
        if timer.exists() {
            systemctl(&["disable", "--now", TIMER])?;
            fs::remove_file(&timer).map_err(|e| io_err(&timer, e))?;
            systemctl(&["daemon-reload"])?;
        }
        return Ok("automatické zálohy vypnuty".into());
    }

    let expr = calendar_expr(input)?;
    let next = next_elapse(&expr)?;
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
    let service = dir.join(SERVICE);
    if !service.exists() {
        let exe = env::current_exe().map_err(|e| format!("nelze zjistit cestu k programu: {e}"))?;
        // Root has no config of its own, so the unit names the one in use.
        let config = match is_root() {
            true => format!("Environment=\"LAFE_BACKUP_CONFIG={}\"\n", config_path()?.display()),
            false => String::new(),
        };
        let unit = format!(
            "[Unit]\nDescription=Záloha lafe-backup (nastavení: lafe-backup show)\n\n\
             [Service]\nType=oneshot\nNice=10\n{config}ExecStart=\"{}\" run\n",
            exe.display()
        );
        fs::write(&service, unit).map_err(|e| io_err(&service, e))?;
    }
    // Intervals under an hour must fire on time: no jitter, no coalescing.
    let timing = if expr.starts_with("*:") {
        "AccuracySec=1s"
    } else {
        "RandomizedDelaySec=10min"
    };
    let unit = format!(
        "[Unit]\nDescription=Automatická záloha lafe-backup\n\n\
         [Timer]\nOnCalendar={expr}\n{timing}\nPersistent=true\n\n\
         [Install]\nWantedBy=timers.target\n"
    );
    fs::write(&timer, unit).map_err(|e| io_err(&timer, e))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", TIMER])?;
    systemctl(&["restart", TIMER])?;
    Ok(format!("četnost: {expr} (další běh {next})"))
}

/// The calendar expression of the installed timer, if there is one.
fn current_schedule() -> Option<String> {
    let timer = unit_dir().ok()?.join(TIMER);
    let text = fs::read_to_string(timer).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("OnCalendar="))
        .map(str::to_string)
}

/// The invoking user's own timer, while no system timer has replaced it.
fn invoker_schedule() -> Option<(String, String)> {
    let inv = invoker()?;
    if Path::new(SYSTEM_UNITS).join(TIMER).exists() {
        return None;
    }
    let text = fs::read_to_string(inv.home.join(".config/systemd/user").join(TIMER)).ok()?;
    let expr = text.lines().find_map(|l| l.strip_prefix("OnCalendar="))?;
    Some((expr.to_string(), inv.name))
}

fn schedule_summary() -> String {
    if let Some((expr, user)) = invoker_schedule() {
        return format!("{expr} (timer uživatele {user}, běží bez práv roota)");
    }
    match current_schedule() {
        None => "vypnuto".into(),
        Some(expr) if systemctl(&["is-enabled", "--quiet", TIMER]).is_ok() => expr,
        Some(expr) => format!("{expr} (timer není zapnutý!)"),
    }
}

fn show(path: &Path, cfg: &Config) {
    println!("Nastavení:  {}", path.display());
    match &cfg.dest {
        Some(dest) => println!("Cíl:        {}", dest.display()),
        None => println!("Cíl:        (nenastaveno)"),
    }
    if let Some(mp) = &cfg.require_mount {
        println!("Mount:      {}", mp.display());
    }
    println!("Jméno:      {}", cfg.name);
    match cfg.keep {
        0 => println!("Ponechat:   vše"),
        n => println!("Ponechat:   posledních {n}"),
    }
    println!("Četnost:    {}", schedule_summary());
    if !cfg.excludes.is_empty() {
        println!("Vynechat:   {}", cfg.excludes.join(", "));
    }
    println!("Zdroje:");
    for src in &cfg.sources {
        let missing = fs::symlink_metadata(src).is_err();
        println!(
            "  {}{}",
            src.display(),
            if missing { "  (neexistuje!)" } else { "" }
        );
    }
    if cfg.sources.is_empty() {
        println!("  (žádné)");
    }
}

fn backup(cfg: &Config, dry_run: bool) -> Result<(), String> {
    let dest = cfg
        .dest
        .as_deref()
        .ok_or("není nastaven cíl (lafe-backup dest <složka>)")?;
    if cfg.sources.is_empty() {
        return Err("není co zálohovat (lafe-backup add <cesta>)".into());
    }

    if let Some(mp) = &cfg.require_mount {
        match is_mountpoint(mp) {
            Ok(true) => {}
            Ok(false) => return Err(format!("{} není připojeno", mp.display())),
            Err(e) => return Err(format!("nelze ověřit připojení {}: {e}", mp.display())),
        }
    }

    if dry_run {
        let mut walker = Walker::<io::Sink> {
            cfg,
            dest,
            tar: None,
            stats: Stats::default(),
        };
        for src in &cfg.sources {
            walker.add(src).map_err(|e| e.to_string())?;
        }
        let s = walker.stats;
        println!(
            "dry-run: {} položek, {}, přeskočeno {}",
            s.files,
            human(s.bytes),
            s.skipped
        );
        return Ok(());
    }

    fs::create_dir_all(dest).map_err(|e| format!("nelze vytvořit {}: {e}", dest.display()))?;
    let file_name = format!("{}-{}.tar.gz", cfg.name, timestamp(SystemTime::now()));
    let final_path = dest.join(&file_name);
    let partial_path = dest.join(format!("{file_name}.partial"));

    let write = || -> io::Result<Stats> {
        let out = BufWriter::new(File::create(&partial_path)?);
        let mut tar = tar::Builder::new(GzEncoder::new(out, Compression::default()));
        tar.follow_symlinks(false);
        let mut walker = Walker {
            cfg,
            dest,
            tar: Some(tar),
            stats: Stats::default(),
        };
        for src in &cfg.sources {
            walker.add(src)?;
        }
        let Some(tar) = walker.tar else { unreachable!() };
        let out = tar.into_inner()?.finish()?;
        let file = out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        fs::rename(&partial_path, &final_path)?;
        Ok(walker.stats)
    };

    let stats = write().map_err(|e| {
        let _ = fs::remove_file(&partial_path);
        format!("zápis {} selhal: {e}", final_path.display())
    })?;
    if stats.files == 0 {
        let _ = fs::remove_file(&final_path);
        return Err("nic nebylo zazálohováno (žádný zdroj není čitelný)".into());
    }

    let size = fs::metadata(&final_path).map_or(0, |m| m.len());
    println!(
        "hotovo: {} ({} položek, {} -> {}, přeskočeno {})",
        final_path.display(),
        stats.files,
        human(stats.bytes),
        human(size),
        stats.skipped
    );

    prune(cfg, dest).map_err(|e| format!("mazání starých záloh selhalo: {e}"))
}

fn run(args: &[String]) -> Result<(), String> {
    let Some((cmd, rest)) = args.split_first() else {
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            let path = config_path()?;
            let cfg = load_config(&path)?;
            return tui::run(path, cfg);
        }
        println!("{USAGE}");
        return Ok(());
    };
    if matches!(cmd.as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(());
    }
    let path = config_path()?;
    let mut cfg = load_config(&path)?;
    let need_args = || match rest {
        [] => Err(format!("příkaz {cmd} vyžaduje argument")),
        _ => Ok(rest),
    };
    let one_arg = || match rest {
        [v] => Ok(v.as_str()),
        _ => Err(format!("příkaz {cmd} vyžaduje právě jeden argument")),
    };

    match cmd.as_str() {
        "add" => {
            for arg in need_args()? {
                let src = absolute(arg)?;
                fs::symlink_metadata(&src).map_err(|e| format!("{}: {e}", src.display()))?;
                if cfg.sources.contains(&src) {
                    println!("už je v seznamu: {}", src.display());
                } else {
                    println!("přidáno: {}", src.display());
                    cfg.sources.push(src);
                }
            }
        }
        "remove" => {
            for arg in need_args()? {
                let src = absolute(arg)?;
                let before = cfg.sources.len();
                cfg.sources.retain(|s| *s != src);
                if cfg.sources.len() == before {
                    return Err(format!("není v seznamu: {}", src.display()));
                }
                println!("odebráno: {}", src.display());
            }
        }
        "dest" => {
            let dest = absolute(one_arg()?)?;
            cfg.require_mount = mount_of(&dest);
            match &cfg.require_mount {
                Some(mp) => println!(
                    "cíl: {} (záloha poběží jen s připojeným {})",
                    dest.display(),
                    mp.display()
                ),
                None => println!("cíl: {}", dest.display()),
            }
            cfg.dest = Some(dest);
        }
        "exclude" => {
            for name in need_args()? {
                if name.contains(['/', '\n']) {
                    return Err(format!("exclude je jméno souboru, ne cesta: {name:?}"));
                }
                if !cfg.excludes.contains(name) {
                    cfg.excludes.push(name.clone());
                }
            }
        }
        "unexclude" => {
            for name in need_args()? {
                cfg.excludes.retain(|e| e != name);
            }
        }
        "keep" => cfg.keep = parse_keep(one_arg()?)?,
        "name" => {
            let name = one_arg()?;
            check_name(name)?;
            cfg.name = name.to_string();
        }
        "schedule" => {
            println!("{}", set_schedule(need_args()?.join(" ").trim())?);
            return Ok(());
        }
        "show" | "list" => {
            show(&path, &cfg);
            return Ok(());
        }
        "run" => {
            let dry_run = match rest {
                [] => false,
                [flag] if flag == "--dry-run" => true,
                _ => return Err("run přijímá jen volbu --dry-run".into()),
            };
            return backup(&cfg, dry_run);
        }
        other => return Err(format!("neznámý příkaz: {other} (viz lafe-backup --help)")),
    }
    save_config(&path, &cfg)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
