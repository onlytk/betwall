use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Casino {
    pub id: String,
    pub label: String,
    pub domain: String,
    #[serde(default)]
    pub blocked_all: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GameEntry {
    pub id: String,
    #[serde(default)]
    pub casino: String,
    pub label: String,
    pub url_pattern: String,
    #[serde(default)]
    pub blocked: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_casinos")]
    pub casinos: Vec<Casino>,
    #[serde(default = "default_games")]
    pub games: Vec<GameEntry>,
    #[serde(default)]
    pub totp_secret_b32: Option<String>,
    #[serde(default)]
    pub setup_complete: bool,
}

fn default_enabled() -> bool {
    true
}

pub fn default_casinos() -> Vec<Casino> {
    let rows: &[(&str, &str, &str)] = &[
        ("stake", "Stake", "stake.com"),
        ("shuffle", "Shuffle", "shuffle.com"),
        ("bcgame", "BC.Game", "bc.game"),
        ("rainbet", "Rainbet", "rainbet.com"),
        ("roobet", "Roobet", "roobet.com"),
        ("gamdom", "Gamdom", "gamdom.com"),
        ("thrill", "Thrill", "thrill.com"),
        ("razed", "Razed", "razed.com"),
        ("chips", "Chips", "chips.gg"),
        ("shock", "Shock", "shock.com"),
        ("menace", "Menace", "menace.bet"),
        ("metawin", "MetaWin", "metawin.com"),
        ("yeet", "Yeet", "yeet.com"),
        ("winna", "Winna", "winna.com"),
        ("acebet", "Acebet", "acebet.com"),
        ("500casino", "500 Casino", "500.casino"),
        ("spartans", "Spartans", "spartans.com"),
        ("degen", "Degen", "degen.com"),
    ];
    rows.iter()
        .map(|(id, label, domain)| Casino {
            id: (*id).into(),
            label: (*label).into(),
            domain: (*domain).into(),
            blocked_all: false,
        })
        .collect()
}

pub fn default_games() -> Vec<GameEntry> {
    let mut out = Vec::new();

    let stake: &[(&str, &str)] = &[
        ("dice", "Dice"),
        ("limbo", "Limbo"),
        ("mines", "Mines"),
        ("plinko", "Plinko"),
        ("crash", "Crash"),
        ("hilo", "Hilo"),
        ("keno", "Keno"),
        ("wheel", "Wheel"),
        ("blackjack", "Blackjack"),
        ("baccarat", "Baccarat"),
        ("roulette", "Roulette"),
        ("diamonds", "Diamonds"),
        ("dragon-tower", "Dragon Tower"),
        ("slide", "Slide"),
        ("video-poker", "Video Poker"),
    ];
    for (slug, label) in stake {
        out.push(GameEntry {
            id: format!("stake-{slug}"),
            casino: "stake".into(),
            label: (*label).into(),
            url_pattern: format!("stake.com/casino/games/{slug}"),
            blocked: false,
        });
    }

    let shuffle: &[(&str, &str)] = &[
        ("dice", "Dice"),
        ("mines", "Mines"),
        ("plinko", "Plinko"),
        ("limbo", "Limbo"),
        ("keno", "Keno"),
        ("baccarat", "Baccarat"),
        ("blackjack", "Blackjack"),
        ("chicken", "Chicken"),
        ("tower", "Tower"),
        ("crash", "Crash"),
        ("hilo", "Hilo"),
        ("wheel", "Wheel"),
        ("roulette", "Roulette"),
    ];
    for (slug, label) in shuffle {
        out.push(GameEntry {
            id: format!("shuffle-{slug}"),
            casino: "shuffle".into(),
            label: (*label).into(),
            url_pattern: format!("shuffle.com/games/originals/{slug}"),
            blocked: false,
        });
    }

    let roobet: &[(&str, &str)] = &[
        ("dice", "Dice"),
        ("mines", "Mines"),
        ("plinko", "Plinko"),
        ("crash", "Crash"),
        ("keno", "Keno"),
        ("towers", "Towers"),
        ("slide", "Slide"),
        ("mission-uncrossable", "Mission Uncrossable"),
    ];
    for (slug, label) in roobet {
        out.push(GameEntry {
            id: format!("roobet-{slug}"),
            casino: "roobet".into(),
            label: (*label).into(),
            url_pattern: format!("roobet.com/casino/game/{slug}"),
            blocked: false,
        });
    }

    let thrill: &[(&str, &str)] = &[
        ("limbo", "Limbo"),
        ("blackjack", "Blackjack"),
        ("dice", "Dice"),
        ("mines", "Mines"),
        ("crash", "Crash"),
        ("plinko", "Plinko"),
    ];
    for (slug, label) in thrill {
        out.push(GameEntry {
            id: format!("thrill-{slug}"),
            casino: "thrill".into(),
            label: (*label).into(),
            url_pattern: format!("thrill.com/casino/play/thrill-{slug}"),
            blocked: false,
        });
    }

    out
}

impl Config {
    pub fn fresh() -> Self {
        Self {
            enabled: true,
            casinos: default_casinos(),
            games: default_games(),
            totp_secret_b32: None,
            setup_complete: false,
        }
    }
}

pub fn config_path() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("betwall");
    let _ = fs::create_dir_all(&p);
    p.push("config.json");
    p
}

pub type SharedConfig = Arc<RwLock<Config>>;

pub fn load() -> SharedConfig {
    let path = config_path();
    let mut cfg: Config = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Config>(&s).ok())
        .unwrap_or_else(Config::fresh);
    if cfg.casinos.is_empty() && !cfg.setup_complete {
        cfg.casinos = default_casinos();
    }
    Arc::new(RwLock::new(cfg))
}

pub fn save(cfg: &Config) {
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(config_path(), s);
    }
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.to_lowercase().chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

pub fn normalize_pattern(s: &str) -> String {
    let trimmed = s.trim();
    let lower = trimmed.to_lowercase();
    let stripped = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    stripped.trim_start_matches("www.").trim_end_matches('/').to_string()
}

pub fn active_patterns(cfg: &Config) -> Vec<String> {
    if !cfg.enabled {
        return Vec::new();
    }
    let mut out = Vec::new();
    let blocked_all: std::collections::HashSet<&str> = cfg
        .casinos
        .iter()
        .filter(|c| c.blocked_all)
        .map(|c| c.id.as_str())
        .collect();
    for c in cfg.casinos.iter().filter(|c| c.blocked_all) {
        out.push(c.domain.to_lowercase());
    }
    for g in &cfg.games {
        if g.blocked && !blocked_all.contains(g.casino.as_str()) {
            out.push(g.url_pattern.to_lowercase());
        }
    }
    out
}
