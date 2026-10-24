//! The freedesktop.org Desktop Entry format, as far as the autostart file
//! needs it: the string-value escapes, the quoting of an `Exec` key's
//! program, and reading booleans from the `[Desktop Entry]` group.
//!
//! An `Exec` value goes through two layers. The Exec rules quote the
//! program in double quotes, with a backslash before each `"`, backtick,
//! `$` and backslash, and write a literal `%` as `%%`; the key file's
//! string rules then double every backslash, so one backslash in a path is
//! four in the file. Plain functions, so every system runs their tests.

use std::path::{Path, PathBuf};

/// The group every key here belongs to.
pub const GROUP: &str = "[Desktop Entry]";

/// The characters the Exec rules want escaped inside double quotes.
const QUOTED_ESCAPES: [char; 4] = ['"', '`', '$', '\\'];

/// The Exec value that starts `exe` with `args` (which need no quoting),
/// before the string escapes ([`escape_value`]). A path with `=` or a
/// control character can't be written: the format doesn't allow `=` in
/// the program, and a control character can't be a key's value.
pub fn exec_command(exe: &Path, args: &[&str]) -> Result<String, String> {
    let path = exe
        .to_str()
        .ok_or_else(|| format!("the path {} isn't UTF-8", exe.display()))?;
    if path.contains('=') {
        return Err(format!(
            "the path {path} holds '=', which an Exec key can't"
        ));
    }
    if path.chars().any(char::is_control) {
        return Err("the path holds a control character".to_owned());
    }
    let mut command = String::with_capacity(path.len() + 2);
    command.push('"');
    for c in path.chars() {
        if QUOTED_ESCAPES.contains(&c) {
            command.push('\\');
        }
        command.push(c);
    }
    command.push('"');
    let mut command = command.replace('%', "%%");
    for arg in args {
        command.push(' ');
        command.push_str(arg);
    }
    Ok(command)
}

/// The program an Exec value starts: the quoted path with its escapes
/// undone, or the first word when it isn't quoted; `None` for a quote that
/// never closes.
pub fn exec_exe(command: &str) -> Option<PathBuf> {
    let command = command.trim_start();
    let mut path = String::new();
    let mut chars = command.chars().peekable();
    if command.starts_with('"') {
        chars.next();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => path.push(chars.next()?),
                '%' if chars.peek() == Some(&'%') => {
                    chars.next();
                    path.push('%');
                }
                c => path.push(c),
            }
        }
    } else {
        let word = command.split_whitespace().next()?;
        path = word.replace("%%", "%");
    }
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// `value` as a key file writes it: backslashes doubled, and a newline,
/// tab or carriage return as `\n`, `\t`, `\r`.
pub fn escape_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// [`escape_value`] undone, with `\s` read as a space as the format says.
/// An unknown escape keeps its backslash.
pub fn unescape_value(raw: &str) -> String {
    let mut value = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            value.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => value.push('\\'),
            Some('n') => value.push('\n'),
            Some('t') => value.push('\t'),
            Some('r') => value.push('\r'),
            Some('s') => value.push(' '),
            Some(other) => {
                value.push('\\');
                value.push(other);
            }
            None => value.push('\\'),
        }
    }
    value
}

/// The raw value of `key` in the `[Desktop Entry]` group of `file`, still
/// escaped. Comments, other groups and localized keys (`Name[de]`) are
/// skipped; spaces around `=` don't count.
pub fn raw_value<'a>(file: &'a str, key: &str) -> Option<&'a str> {
    let mut in_group = false;
    for line in file.lines() {
        let line = line.trim_start();
        if line.starts_with('[') {
            in_group = line.trim_end() == GROUP;
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        if let Some((name, value)) = line.split_once('=')
            && name.trim_end() == key
        {
            return Some(value.trim_start());
        }
    }
    None
}

/// `key` read as a boolean: `Some` only for exactly `true` or `false`.
pub fn bool_value(file: &str, key: &str) -> Option<bool> {
    match raw_value(file, key)?.trim_end() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Through both layers and back, as the autostart file stores it.
    fn round_trip(path: &str) -> Option<PathBuf> {
        let command = exec_command(Path::new(path), &["--background"]).expect("writable");
        let line = format!("{GROUP}\nExec={}\n", escape_value(&command));
        exec_exe(&unescape_value(raw_value(&line, "Exec")?))
    }

    #[test]
    fn a_plain_path_is_quoted_and_followed_by_its_arguments() {
        assert_eq!(
            exec_command(Path::new("/usr/bin/audiosync"), &["--background"]).unwrap(),
            r#""/usr/bin/audiosync" --background"#
        );
    }

    #[test]
    fn reserved_characters_get_a_backslash_and_percent_is_doubled() {
        let command = exec_command(Path::new(r#"/a b/"q"/`c`/$d/e\f/50%"#), &[]).unwrap();
        assert_eq!(command, r#""/a b/\"q\"/\`c\`/\$d/e\\f/50%%""#);
        // The string layer doubles each backslash again: one in the path is four in the file.
        assert_eq!(
            escape_value(&command),
            r#""/a b/\\"q\\"/\\`c\\`/\\$d/e\\\\f/50%%""#
        );
    }

    #[test]
    fn every_awkward_path_reads_back_as_it_was_written() {
        for path in [
            "/usr/bin/audiosync",
            "/home/me/My Apps/AudioSync.AppImage",
            r#"/home/me/"quoted"/audiosync"#,
            "/home/me/$HOME/`id`/audiosync",
            "/home/me/100%/audiosync",
            r"/home/me/back\slash/audiosync",
            r"/home/me/\\double/%%/audiosync",
            "/home/me/ünïcode/audiosync",
        ] {
            assert_eq!(round_trip(path), Some(PathBuf::from(path)), "{path}");
        }
    }

    #[test]
    fn an_equals_sign_or_a_control_character_is_refused() {
        assert!(exec_command(Path::new("/opt/a=b/audiosync"), &[]).is_err());
        assert!(exec_command(Path::new("/opt/a\nb/audiosync"), &[]).is_err());
        assert!(exec_command(Path::new("/opt/a\tb/audiosync"), &[]).is_err());
    }

    #[test]
    fn an_unquoted_program_is_its_first_word() {
        assert_eq!(
            exec_exe("/usr/bin/audiosync --background"),
            Some(PathBuf::from("/usr/bin/audiosync"))
        );
        assert_eq!(exec_exe("  "), None);
        assert_eq!(exec_exe(r#""/usr/bin/audiosync"#), None);
    }

    #[test]
    fn string_escapes_undo_each_other_and_s_is_a_space() {
        let value = "a\\b\nc\td\re";
        assert_eq!(unescape_value(&escape_value(value)), value);
        assert_eq!(unescape_value(r"a\sb"), "a b");
        assert_eq!(unescape_value(r"a\qb\"), r"a\qb\");
    }

    #[test]
    fn keys_are_read_from_the_desktop_entry_group_only() {
        let file = "# a comment\n[Other]\nHidden=true\n[Desktop Entry]\nName[de]=X\n\
                    Name = AudioSync\n#Hidden=true\nX-GNOME-Autostart-enabled=false\n";
        assert_eq!(raw_value(file, "Name"), Some("AudioSync"));
        assert_eq!(bool_value(file, "Hidden"), None);
        assert_eq!(bool_value(file, "X-GNOME-Autostart-enabled"), Some(false));
        assert_eq!(bool_value("[Desktop Entry]\nHidden=yes\n", "Hidden"), None);
        assert_eq!(
            bool_value("[Desktop Entry]\nHidden=true\n", "Hidden"),
            Some(true)
        );
    }
}
