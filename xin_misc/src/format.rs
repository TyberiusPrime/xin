use std::ffi::OsStr;
use std::process::Command;

/// Quote a string for a POSIX shell only when needed, so the output is copy-pasteable.
fn shell_quote(s: &OsStr) -> String {
    let s = s.to_string_lossy();
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    if safe {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Print a Command like:
///
/// ```text
/// cmd \
///   --arg1 \
///   --arg2 \
///   --arg3 value3 \
///   --arg4 value4
/// ```
///
/// A flag is paired with the following argument on the same line when that
/// argument doesn't look like a flag itself. `--key=value` stays on one line.
pub fn pretty_debug_cmd(cmd: &Command) {
    let mut lines: Vec<String> = Vec::new();

    // First line: optional cwd, env vars, then the program.
    let mut head = String::new();
    if let Some(dir) = cmd.get_current_dir() {
        head.push_str(&format!("cd {} && ", shell_quote(dir.as_os_str())));
    }
    for (k, v) in cmd.get_envs() {
        if let Some(v) = v {
            head.push_str(&format!("{}={} ", k.to_string_lossy(), shell_quote(v)));
        }
    }
    head.push_str(&shell_quote(cmd.get_program()));
    lines.push(head);

    // Remaining lines: each flag starts a new line, and every following
    // non-flag arg stays on that line until the next flag.
    let mut in_flag = false;
    for arg in cmd.get_args().map(shell_quote) {
        if arg.starts_with('-') {
            lines.push(arg);
            in_flag = true;
        } else if in_flag {
            let last = lines.last_mut().unwrap();
            last.push(' ');
            last.push_str(&arg);
        } else {
            // Positional args before the first flag get their own lines.
            lines.push(arg);
        }
    }

    eprintln!("{}", lines.join(" \\\n  "));
}
