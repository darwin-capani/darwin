//! WS4a — the "Uninstall DARWIN" affordance (the SETTINGS / System tab).
//!
//! WHAT THIS IS: one Tauri command (`uninstall_open`) that OPENS Terminal.app
//! running the installed `uninstall.sh` so the user completes the script's TWO
//! typed confirmations IN A REAL TERMINAL.
//!
//! WHY THIS SHAPE (the safety invariant — do NOT weaken it): `uninstall.sh` is a
//! deliberately destructive, two-step typed-confirmation flow ("Delete DARWIN
//! completely? (yes/no)" then "Are you ABSOLUTELY sure? (yes/no)", failing safe
//! to NO on anything else). A button must NEVER auto-run that from a single
//! click. So this command does NOT execute the uninstaller and does NOT pass any
//! "yes"/"--force"/auto-confirm flag — it merely OPENS Terminal.app on the
//! script, and the user types both confirmations themselves in the terminal. The
//! script keeps full control of the destructive decision; the HUD only launches a
//! terminal pointed at it. (We pass NO arguments at all, so the script runs in its
//! normal interactive two-step mode.)
//!
//! PATH SAFETY: the script path is resolved from the SAME DARWIN root the command
//! channel + self-heal use (`heal::resolve_root_for_command`) — never a path from
//! the frontend (the command takes NO argument). We verify the file exists and
//! pass it to Terminal as a quoted POSIX path with embedded quotes escaped, so a
//! root containing spaces (the installed home is
//! `~/Library/Application Support/DARWIN`) is handled correctly and nothing can be
//! injected via the path.

use std::path::PathBuf;
use std::process::Stdio;

use serde::Serialize;
use tokio::process::Command;

/// The outcome surfaced to the HUD. `opened` is true only when Terminal.app was
/// actually launched on the uninstaller; `detail` is a short human line (never a
/// secret — the only material here is a public file path).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UninstallOpen {
    pub opened: bool,
    pub detail: String,
}

/// Resolve the installed `uninstall.sh` under the DARWIN root. Reuses the shared
/// resolver (DARWIN_ROOT env / the exe-cwd upward walk to the
/// scripts/apply_heal.sh + config/darwin.toml markers) so we land on the SAME
/// root the rest of the shell uses; the installed root is
/// `~/Library/Application Support/DARWIN`. We never accept a path from the caller.
fn uninstall_script_path() -> Result<PathBuf, String> {
    let root = crate::heal::resolve_root_for_command()?;
    Ok(root.join("uninstall.sh"))
}

/// Wrap a path in SHELL single quotes so it stays exactly ONE argument: a space,
/// a `;`, a `$`, a `&&` — nothing inside `'...'` is interpreted, and an embedded
/// `'` is closed / backslash-escaped / reopened. NO argument is ever appended, so
/// the script always runs its interactive two-step typed-confirmation flow; the
/// user does the confirming.
///
/// EXTRACTED so the tests can call it. They used to RE-TYPE this expression
/// inline against a local literal and never reference a single production symbol,
/// so deleting the escaping (or appending an auto-confirm flag) in
/// `uninstall_open` left them green — the module's stated safety invariant had no
/// coverage at all despite two tests named for it.
pub(crate) fn shell_single_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// The HONEST not-an-install outcome: `opened:false` plus a line naming the path
/// we looked at. EXTRACTED so a test can assert the real branch instead of the
/// tautology `open_result_shape` used to run (construct an `UninstallOpen` and
/// assert the field it set one line earlier).
pub(crate) fn script_not_found(script: &std::path::Path) -> UninstallOpen {
    UninstallOpen {
        opened: false,
        detail: format!(
            "uninstall.sh not found at {} — this looks like a dev/source tree, not an install. Run it from the installed DARWIN home.",
            script.display()
        ),
    }
}

/// The exact AppleScript `uninstall_open` runs: bring Terminal forward and run
/// the ALREADY shell-quoted command, escaped for the AppleScript string literal
/// it is embedded in. EXTRACTED for the same reason as `shell_single_quote`.
pub(crate) fn terminal_do_script(shell_quoted: &str) -> String {
    format!(
        "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
        escape_applescript(shell_quoted)
    )
}

/// THE WHOLE COMPOSITION `uninstall_open` hands to osascript: quote the script
/// path, then wrap it in the Terminal `do script` AppleScript. This is the single
/// place the command is BUILT, so a test that asserts "the quoted path and NO
/// argument" over this function really does guard the no-auto-confirm invariant.
///
/// (Testing `terminal_do_script(shell_single_quote(p))` from the test body was
/// NOT enough: it re-composed the two helpers itself, so appending `--yes` inside
/// `uninstall_open` still slipped through — the very defect the old tests had,
/// one layer up. Mutation-checked.)
pub(crate) fn uninstall_applescript(script: &std::path::Path) -> String {
    terminal_do_script(&shell_single_quote(&script.to_string_lossy()))
}

/// Escape a string for safe embedding inside an AppleScript DOUBLE-quoted string
/// literal: a backslash and a double quote are the only metacharacters inside an
/// AppleScript "..." string, so escaping both makes any path inert. The path is
/// then handed to Terminal's `do script`, which runs it as a shell command — the
/// path is itself quoted in that shell command too (see `uninstall_open`), so a
/// space or shell metacharacter in the path can never split into extra args.
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// OPEN Terminal.app running the installed `uninstall.sh`. Does NOT run the
/// uninstaller itself and passes NO auto-confirm — the user types the script's
/// two confirmations in the terminal. Honest about failure (script missing /
/// Terminal could not be launched) rather than pretending it ran.
///
/// The command takes NO argument from the frontend (the path is resolved
/// server-side), so there is no injection surface. We shell `osascript` to tell
/// Terminal to `do script "<path>"` — the same osascript path the folder picker
/// already uses, so no new dependency / capability is added.
#[tauri::command]
pub async fn uninstall_open() -> Result<UninstallOpen, String> {
    let script = uninstall_script_path()?;
    if !script.is_file() {
        return Ok(script_not_found(&script));
    }

    // Build the shell command Terminal will run. The script path is wrapped in
    // single quotes (with any embedded single quote closed/escaped) so a path with
    // spaces stays ONE argument and no shell metacharacter is interpreted. We pass
    // NO arguments to the script -> it runs its normal interactive two-step
    // typed-confirmation flow; the user does the confirming.
    // The full AppleScript: bring Terminal forward and run the (single-quoted)
    // script path. The whole `do script` argument is an AppleScript string literal,
    // so it is escaped for that context too. Built by `uninstall_applescript` —
    // the ONE place the command is composed, so the no-auto-confirm test really
    // guards this call site.
    let applescript = uninstall_applescript(&script);

    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&applescript)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("could not launch Terminal: {e}"))?;

    if output.status.success() {
        Ok(UninstallOpen {
            opened: true,
            detail:
                "Opened Terminal running uninstall.sh. Complete the two typed confirmations there — \
                 nothing is removed until you type 'yes' to BOTH prompts. Type 'no' (or close the \
                 window) to cancel."
                    .into(),
        })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(UninstallOpen {
            opened: false,
            detail: format!(
                "could not open Terminal: {}",
                stderr.trim().lines().next().unwrap_or("no GUI session")
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_escaping_neutralizes_quotes_and_backslashes() {
        assert_eq!(escape_applescript("plain"), "plain");
        assert_eq!(escape_applescript(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_applescript(r"a\b"), r"a\\b");
        // A path that tries to close the AppleScript string + inject is fully
        // escaped. The real invariant: in the escaped output EVERY double-quote is
        // immediately preceded by a backslash, so none can terminate the
        // surrounding "..." literal early. We verify that property directly by
        // scanning the bytes (a `"` at index 0, or one whose predecessor is not a
        // backslash, would be an un-neutralized closing quote).
        let hostile = r#"/x"; rm -rf ~; echo ""#;
        let esc = escape_applescript(hostile);
        let bytes = esc.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'"' {
                assert!(i > 0 && bytes[i - 1] == b'\\', "un-escaped quote at {i}: {esc}");
            }
        }
        // And it did escape the quotes (the hostile input had two).
        assert!(esc.contains("\\\""), "quotes are escaped: {esc}");
    }

    /* These two used to RE-TYPE `format!("'{}'", path.replace('\'', "'\\''"))`
       inline against a local literal and assert the result equals what they had
       just written. Neither body referenced a single symbol from `super::*`, so
       corrupting or deleting the quoting in `uninstall_open` left both green —
       and `!quoted.contains("yes")` was checking a hard-coded path constant, not
       the no-auto-confirm invariant it is named for. They now call the PRODUCTION
       `shell_single_quote`. */

    #[test]
    fn shell_single_quoting_keeps_a_spaced_path_as_one_arg() {
        // The installed home has a space; single-quoting must preserve it whole.
        let path = "/Users/x/Library/Application Support/DARWIN/uninstall.sh";
        assert_eq!(
            shell_single_quote(path),
            "'/Users/x/Library/Application Support/DARWIN/uninstall.sh'"
        );
    }

    #[test]
    fn shell_single_quoting_escapes_an_embedded_single_quote() {
        let path = "/Users/o'brien/DARWIN/uninstall.sh";
        // The embedded ' is closed, escaped, and reopened — a valid shell literal.
        assert_eq!(shell_single_quote(path), "'/Users/o'\\''brien/DARWIN/uninstall.sh'");
    }

    /// A shell metacharacter in the path cannot break out of the single quotes,
    /// so it can never become a second command.
    #[test]
    fn shell_single_quoting_neutralizes_metacharacters() {
        let hostile = "/Users/x/D; rm -rf ~/ && echo $HOME/uninstall.sh";
        let q = shell_single_quote(hostile);
        assert!(q.starts_with('\'') && q.ends_with('\''));
        // The ONLY quotes in the output are the wrapping pair (no `'` in the input
        // to escape), so nothing inside is interpreted by the shell.
        assert_eq!(q.matches('\'').count(), 2, "{q}");
    }

    /// THE SAFETY INVARIANT THIS MODULE EXISTS FOR: the built AppleScript carries
    /// the quoted script path and NOTHING else — no auto-confirm flag is ever
    /// appended, so the uninstaller always runs its interactive two-step typed
    /// confirmation. Built from the PRODUCTION helpers, so appending an argument
    /// in `uninstall_open` turns this red.
    #[test]
    fn the_built_applescript_passes_the_script_path_and_no_arguments() {
        let path = "/Users/x/Library/Application Support/DARWIN/uninstall.sh";
        // Build it the way `uninstall_open` does — through the ONE composition
        // function — so appending an argument at that call site turns this red.
        let script = uninstall_applescript(std::path::Path::new(path));
        assert!(script.contains("tell application \\\"Terminal\\\"") || script.contains("tell application \"Terminal\""));
        assert!(script.contains("do script"));
        // The command Terminal runs is exactly the quoted path.
        let start = script.find("do script \"").expect("do script present") + "do script \"".len();
        let end = script[start..].find("\"\n").expect("closing quote") + start;
        let command = &script[start..end];
        assert_eq!(
            command,
            "'/Users/x/Library/Application Support/DARWIN/uninstall.sh'",
            "the command must be the quoted path and NOTHING else"
        );
        // No auto-confirm flag, in any spelling.
        for flag in ["--yes", "-y", "--force", "--no-confirm", "--assume-yes"] {
            assert!(!script.contains(flag), "auto-confirm flag {flag} leaked into: {script}");
        }
    }

    /// `open_result_shape` used to construct an `UninstallOpen` and assert the
    /// field it had just set one line earlier — a pure tautology. Assert the
    /// PRODUCTION not-found branch instead: a dev/source tree must report
    /// `opened:false` and say so, never silently "succeed".
    #[test]
    fn a_missing_uninstall_script_is_an_honest_not_opened() {
        let r = script_not_found(std::path::Path::new("/nope/DARWIN/uninstall.sh"));
        assert!(!r.opened, "a missing script must never report opened");
        assert!(r.detail.contains("uninstall.sh not found"), "{}", r.detail);
        assert!(r.detail.contains("/nope/DARWIN/uninstall.sh"), "{}", r.detail);
        // It must not claim anything ran.
        assert!(!r.detail.to_ascii_lowercase().contains("uninstalled"), "{}", r.detail);
    }
}
