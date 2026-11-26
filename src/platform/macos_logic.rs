//! Чистая (без обращений к ОС) логика macOS-слоя: экранирование, разбор вывода утилит,
//! генерация plist, планирование запуска. Не зависит от платформы, поэтому тесты идут и на
//! Windows. Всё, что вызывает `osascript`/`ps`/`open`, живёт в `mac/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Идентификатор приложения (CFBundleIdentifier, Label LaunchAgent).
pub const BUNDLE_ID: &str = "io.github.eg0rs.presetlauncher";

/// Процессы, которые нельзя закрывать при переключении пресета (имена в нижнем регистре).
pub const PROTECTED: &[&str] = &[
    "finder", "loginwindow", "windowserver", "dock", "systemuiserver", "launchd", "controlcenter",
    "notificationcenter", "coreservicesd", "kernel_task", "spotlight", "universalaccessd",
];

pub fn is_protected(process_name: &str) -> bool {
    PROTECTED.contains(&process_name)
}

/// `<home>/Library/Application Support`.
pub fn app_support_dir(home: &Path) -> PathBuf {
    home.join("Library").join("Application Support")
}

/// Путь к plist LaunchAgent автозапуска.
pub fn launch_agent_path(home: &Path) -> PathBuf {
    home.join("Library").join("LaunchAgents").join(format!("{BUNDLE_ID}.plist"))
}

/// Ссылка вида `схема://…`.
pub fn is_url(path: &str) -> bool {
    path.contains("//")
        && path.split_once("//").is_some_and(|(scheme, _)| scheme.ends_with(':') && scheme.len() > 1 && !scheme.contains('/'))
}

/// Это путь к пакету `.app` (URL вроде `https://linear.app/` — нет)?
pub fn is_app_bundle(path: &str) -> bool {
    !is_url(path.trim()) && path.trim().trim_end_matches('/').to_lowercase().ends_with(".app")
}

/// Имя процесса (нижний регистр) для пути программы: `/Applications/Foo.app` -> `foo`
/// (точное имя исполняемого файла из Info.plist уточняет вызывающая сторона), исполняемый
/// файл (`is_executable`) — его имя. Для URL и документов `None`: такой процесс не отслеживается
/// и не закрывается (как URL на Windows), пока не задан явный `process_name`.
pub fn process_name_from_path(path: &str, is_executable: bool) -> Option<String> {
    let trimmed = path.trim().trim_end_matches('/');
    if is_url(trimmed) {
        return None;
    }
    let p = Path::new(trimmed);
    let name = if is_app_bundle(trimmed) {
        p.file_stem()?.to_string_lossy().to_lowercase()
    } else if is_executable {
        p.file_name()?.to_string_lossy().to_lowercase()
    } else {
        return None;
    };
    (!name.is_empty()).then_some(name)
}

/// Разбор вывода `ps -axo pid=,comm=`: имя процесса (нижний регистр, без каталога) -> pid.
/// Процесс `skip_pid` (мы сами) пропускается.
pub fn parse_ps(output: &str, skip_pid: u32) -> HashMap<String, Vec<u32>> {
    let mut map: HashMap<String, Vec<u32>> = HashMap::new();
    for line in output.lines() {
        let line = line.trim_start();
        let Some((pid, comm)) = line.split_once(char::is_whitespace) else { continue };
        let Ok(pid) = pid.parse::<u32>() else { continue };
        let comm = comm.trim();
        if pid == skip_pid || comm.is_empty() {
            continue;
        }
        let name = comm.rsplit('/').next().unwrap_or(comm).to_lowercase();
        if !name.is_empty() {
            map.entry(name).or_default().push(pid);
        }
    }
    map
}

/// Разбор `osascript -e 'get volume settings'`:
/// `output volume:50, input volume:75, alert volume:100, output muted:false`.
pub fn parse_volume_settings(s: &str) -> Result<(u8, bool), String> {
    let mut volume: Option<String> = None;
    let mut muted: Option<String> = None;
    for part in s.trim().split(',') {
        let Some((k, v)) = part.split_once(':') else { continue };
        match k.trim() {
            "output volume" => volume = Some(v.trim().to_string()),
            "output muted" => muted = Some(v.trim().to_string()),
            _ => {}
        }
    }
    let volume = volume.ok_or("в ответе нет «output volume»")?;
    let vol = volume
        .parse::<u32>()
        .map_err(|_| format!("устройство вывода не поддерживает регулировку громкости («{volume}»)"))?
        .min(100) as u8;
    // `output muted` может быть «missing value» (устройство без mute) — считаем «не выключен».
    Ok((vol, muted.as_deref() == Some("true")))
}

/// Аргументы `osascript` для показа уведомления. Заголовок и текст идут отдельными аргументами
/// (`on run argv`), а не склеиваются в код AppleScript, поэтому инъекция невозможна.
/// `--` не даёт заголовку, начинающемуся с «-», превратиться в опцию.
pub fn notification_osascript_args(title: &str, message: &str) -> Vec<String> {
    let clean = |s: &str| -> String { s.chars().filter(|c| *c != '\0').collect() };
    vec![
        "-e".into(),
        "on run argv".into(),
        "-e".into(),
        "display notification (item 2 of argv) with title (item 1 of argv)".into(),
        "-e".into(),
        "end run".into(),
        "--".into(),
        clean(title),
        clean(message),
    ]
}

/// Экранирование текста для XML (plist).
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// plist LaunchAgent: запуск `exe args…` при входе в систему.
pub fn launch_agent_plist(exe: &str, args: &[&str]) -> String {
    let mut lines: Vec<String> = vec![
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>".into(),
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">".into(),
        "<plist version=\"1.0\">".into(),
        "<dict>".into(),
        "  <key>Label</key>".into(),
        format!("  <string>{BUNDLE_ID}</string>"),
        "  <key>ProgramArguments</key>".into(),
        "  <array>".into(),
    ];
    for a in std::iter::once(exe).chain(args.iter().copied()) {
        lines.push(format!("    <string>{}</string>", xml_escape(a)));
    }
    lines.extend(
        [
            "  </array>",
            "  <key>RunAtLoad</key>",
            "  <true/>",
            "  <key>ProcessType</key>",
            "  <string>Interactive</string>",
            "</dict>",
            "</plist>",
        ]
        .map(String::from),
    );
    lines.join("\n") + "\n"
}

/// Делит строку аргументов по пробелам с учётом "двойных" и 'одинарных' кавычек
/// (без подстановок оболочки).
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                has = true;
            }
            (None, c) if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Как запускать программу пресета.
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchPlan {
    /// `open <args>`
    Open(Vec<String>),
    /// Прямой запуск исполняемого файла.
    Direct { program: String, args: Vec<String> },
}

/// `.app` -> `open -a`; URL и документы -> `open`; исполняемый файл -> прямой запуск.
pub fn plan_launch(path: &str, args: &str, is_executable_file: bool) -> LaunchPlan {
    let path = path.trim();
    let args = split_args(args);
    if is_url(path) {
        // URL: аргументы `open` не принимает.
        LaunchPlan::Open(vec![path.to_string()])
    } else if is_app_bundle(path) {
        // `--args` доходят только до нового экземпляра, поэтому с аргументами просим `-n`.
        let mut v: Vec<String> = Vec::new();
        if !args.is_empty() {
            v.push("-n".into());
        }
        v.push("-a".into());
        v.push(path.into());
        if !args.is_empty() {
            v.push("--args".into());
            v.extend(args);
        }
        LaunchPlan::Open(v)
    } else if is_executable_file && !path.contains("://") {
        LaunchPlan::Direct { program: path.into(), args }
    } else {
        // Для URL и документов аргументы не имеют смысла: `open` их не примет.
        LaunchPlan::Open(vec![path.to_string()])
    }
}

/// Код клавиши NX_KEYTYPE_* для системных media-событий; `None` — не поддерживается.
pub fn media_key_code(key: crate::api::MediaKey) -> Option<i64> {
    use crate::api::MediaKey::*;
    match key {
        PlayPause => Some(16), // NX_KEYTYPE_PLAY
        Next => Some(17),      // NX_KEYTYPE_NEXT
        Prev => Some(18),      // NX_KEYTYPE_PREVIOUS
        Stop => None,
    }
}

/// `data1` NSEvent для NX_SYSDEFINED (subtype 8): код клавиши и состояние down/up.
pub fn media_key_data1(code: i64, down: bool) -> i64 {
    (code << 16) | ((if down { 0xA } else { 0xB }) << 8)
}

/// `osascript` для блокировки экрана (Ctrl+Cmd+Q через System Events; нужен Accessibility).
pub const LOCK_SCRIPT: &str = "tell application \"System Events\" to keystroke \"q\" using {control down, command down}";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::MediaKey;

    #[test]
    fn notification_args_keep_text_out_of_script() {
        let evil = "\" & (do shell script \"id\") & \"";
        let a = notification_osascript_args(evil, "-x \\ \"q\"");
        // Весь код скрипта — константы; пользовательский текст только в последних двух аргументах.
        assert_eq!(a[1], "on run argv");
        assert_eq!(a[3], "display notification (item 2 of argv) with title (item 1 of argv)");
        assert_eq!(a[6], "--");
        assert_eq!(a[7], evil);
        assert_eq!(a[8], "-x \\ \"q\"");
        assert!(!a[..7].iter().any(|s| s.contains("shell script")));
        assert_eq!(notification_osascript_args("a\0b", "c")[7], "ab");
    }

    #[test]
    fn volume_parse() {
        let s = "output volume:42, input volume:75, alert volume:100, output muted:false\n";
        assert_eq!(parse_volume_settings(s), Ok((42, false)));
        assert_eq!(parse_volume_settings("output volume:0, input volume:1, alert volume:1, output muted:true"), Ok((0, true)));
        assert!(parse_volume_settings("output volume:missing value, input volume:1, alert volume:1, output muted:missing value").is_err());
        assert!(parse_volume_settings("").is_err());
        assert_eq!(parse_volume_settings("output volume:100, output muted:missing value"), Ok((100, false)));
    }

    #[test]
    fn ps_parse() {
        let out = "  1 /sbin/launchd\n 501 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome\n 502 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome\n  77 -zsh\nbad line\n  9 /me\n";
        let m = parse_ps(out, 9);
        assert_eq!(m["launchd"], vec![1]);
        assert_eq!(m["google chrome"], vec![501, 502]);
        assert_eq!(m["-zsh"], vec![77]);
        assert!(!m.contains_key("me"));
    }

    #[test]
    fn process_names() {
        assert_eq!(process_name_from_path("/Applications/Google Chrome.app", false).as_deref(), Some("google chrome"));
        assert_eq!(process_name_from_path("/Applications/Foo.app/", false).as_deref(), Some("foo"));
        assert_eq!(process_name_from_path("/usr/local/bin/Tool", true).as_deref(), Some("tool"));
        assert_eq!(process_name_from_path("/Users/u/doc.pdf", false), None);
        assert_eq!(process_name_from_path("https://linear.app/", false), None);
        assert_eq!(process_name_from_path("https://linear.app/", true), None);
        assert!(is_protected("finder") && !is_protected("safari"));
    }

    #[test]
    fn plist_is_escaped_and_well_formed() {
        let p = launch_agent_plist("/Apps/A&B <x>.app/Contents/MacOS/pl", &["--autostart"]);
        assert!(p.contains("<string>io.github.eg0rs.presetlauncher</string>"));
        assert!(p.contains("<string>/Apps/A&amp;B &lt;x&gt;.app/Contents/MacOS/pl</string>"));
        assert!(p.contains("<string>--autostart</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(p.starts_with("<?xml"));
        assert_eq!(
            launch_agent_path(Path::new("/Users/u")),
            Path::new("/Users/u/Library/LaunchAgents/io.github.eg0rs.presetlauncher.plist")
        );
        assert_eq!(app_support_dir(Path::new("/Users/u")), Path::new("/Users/u/Library/Application Support"));
    }

    #[test]
    fn args_split() {
        assert_eq!(split_args("--new-window \"https://a b\" 'x y' z"), vec!["--new-window", "https://a b", "x y", "z"]);
        assert_eq!(split_args("  "), Vec::<String>::new());
        assert_eq!(split_args("\"\" a"), vec!["", "a"]);
    }

    #[test]
    fn launch_plans() {
        assert_eq!(plan_launch("/Applications/Safari.app", "", false), LaunchPlan::Open(vec!["-a".into(), "/Applications/Safari.app".into()]));
        assert_eq!(
            plan_launch("/Applications/Chrome.app", "--new-window x", false),
            LaunchPlan::Open(vec!["-n".into(), "-a".into(), "/Applications/Chrome.app".into(), "--args".into(), "--new-window".into(), "x".into()])
        );
        assert_eq!(plan_launch("/bin/tool", "-v", true), LaunchPlan::Direct { program: "/bin/tool".into(), args: vec!["-v".into()] });
        assert_eq!(plan_launch("https://linear.app/", "", false), LaunchPlan::Open(vec!["https://linear.app/".into()]));
        assert!(is_url("steam://open/games") && !is_url("/Applications/A.app") && !is_app_bundle("https://linear.app/"));
        assert_eq!(plan_launch("https://example.com", "", false), LaunchPlan::Open(vec!["https://example.com".into()]));
        assert_eq!(plan_launch("steam://open/games", "", true), LaunchPlan::Open(vec!["steam://open/games".into()]));
    }

    #[test]
    fn media_keys() {
        assert_eq!(media_key_code(MediaKey::PlayPause), Some(16));
        assert_eq!(media_key_code(MediaKey::Stop), None);
        assert_eq!(media_key_data1(16, true), (16 << 16) | 0xA00);
        assert_eq!(media_key_data1(16, false), (16 << 16) | 0xB00);
    }
}
