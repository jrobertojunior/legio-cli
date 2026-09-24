//! The kinds of line the tool prints: a step heading, a fact, a warning,
//! a key phrase, and a question.

use std::io::{self, BufRead, IsTerminal, Write};

pub fn bold(text: &str) {
    println!("\x1b[1m{text}\x1b[0m");
}

pub fn info(text: &str) {
    println!("  {text}");
}

pub fn warn(text: &str) {
    println!("\x1b[33mWARNING: {text}\x1b[0m");
}

/// A key phrase in a box of its own, the words bold and spaced apart, so
/// the eye finds it among the lines around it. The words are the ones the
/// app shows, in the same order.
pub fn phrase(phrase: &str) {
    let words = phrase.replace('-', "   ");
    let rule = "─".repeat(words.chars().count() + 6);
    println!();
    println!("    ╭{rule}╮");
    println!("    │   \x1b[1;36m{words}\x1b[0m   │");
    println!("    ╰{rule}╯");
    println!();
}

/// A yes/no question on the terminal. Anything but `y` or `yes` is a no,
/// so a stray Enter never approves a key.
///
/// Fails when stdin is not a terminal: the question is the one step that
/// proves a person looked at the key phrase, and a pipe cannot look.
pub fn confirm(question: &str) -> anyhow::Result<bool> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        anyhow::bail!("cannot ask \"{question}\": stdin is not a terminal");
    }
    print!("  {question} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    stdin.lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
