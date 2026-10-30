use crate::ParseError;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Word(String),
    Str(String),
    Var(String),
    /// `.name` directly after a variable, a closing paren or another member.
    Member(String),
    /// `(` directly after a member, which turns the member into a method call.
    CallOpen,
    Pipe,
    Semi,
    Newline,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    /// A trailing `&`, which runs the statement in the background.
    Amp,
    /// `o>`, `e>`, `o+e>` and their `>>` forms, which append.
    Redirect {
        stdout: bool,
        stderr: bool,
        append: bool,
    },
}

fn redirect(word: &str) -> Option<Tok> {
    let (streams, append) = match word.strip_suffix(">>") {
        Some(s) => (s, true),
        None => (word.strip_suffix('>')?, false),
    };
    let (stdout, stderr) = match streams {
        "o" | "out" => (true, false),
        "e" | "err" => (false, true),
        "o+e" | "out+err" | "e+o" => (true, true),
        _ => return None,
    };
    Some(Tok::Redirect { stdout, stderr, append })
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A comma separates items only inside lists and records, so `chmod u+x,go-w` stays one word.
fn is_delimiter(c: char, nested: bool) -> bool {
    c.is_whitespace() || "|;()[]{}".contains(c) || (c == ',' && nested)
}

pub fn lex(src: &str) -> Result<Vec<Tok>, ParseError> {
    Ok(lex_with_positions(src)?.0)
}

/// Tokens and the character offset where each one starts.
pub fn lex_with_positions(src: &str) -> Result<(Vec<Tok>, Vec<usize>), ParseError> {
    let chars: Vec<char> = src.chars().collect();
    let at = |i: usize| chars.get(i).copied();
    let mut toks = Vec::new();
    let mut positions: Vec<usize> = Vec::new();
    let mut token_start = 0;
    let mut nesting = 0i32;
    let mut i = 0;
    // Member access only binds without whitespace, so `$it.name` differs from `$it .name`.
    let mut member_ok = false;

    while let Some(c) = at(i) {
        positions.resize(toks.len(), token_start);
        token_start = i;
        if member_ok && c == '.' && at(i + 1).is_some_and(is_ident) {
            let start = i + 1;
            i = start;
            while at(i).is_some_and(is_ident) {
                i += 1;
            }
            toks.push(Tok::Member(chars[start..i].iter().collect()));
            if at(i) == Some('(') {
                toks.push(Tok::CallOpen);
                i += 1;
                member_ok = false;
            }
            continue;
        }
        member_ok = false;
        match c {
            '\n' => {
                toks.push(Tok::Newline);
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            '#' => {
                while at(i).is_some_and(|c| c != '\n') {
                    i += 1;
                }
            }
            '|' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ':' => {
                toks.push(match c {
                    '|' => Tok::Pipe,
                    ';' => Tok::Semi,
                    '(' => Tok::LParen,
                    ')' => Tok::RParen,
                    '[' => Tok::LBracket,
                    ']' => Tok::RBracket,
                    '{' => Tok::LBrace,
                    '}' => Tok::RBrace,
                    ',' => Tok::Comma,
                    _ => Tok::Colon,
                });
                member_ok = c == ')';
                match c {
                    '[' | '{' => nesting += 1,
                    ']' | '}' => nesting -= 1,
                    _ => {}
                }
                i += 1;
            }
            '"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match at(i) {
                        None => return Err(ParseError::at("unterminated string", token_start)),
                        Some('"') => break,
                        Some('\\') => {
                            i += 1;
                            s.push(match at(i) {
                                Some('n') => '\n',
                                Some('t') => '\t',
                                Some(other) => other,
                                None => return Err(ParseError::at("unterminated string", token_start)),
                            });
                        }
                        Some(other) => s.push(other),
                    }
                    i += 1;
                }
                i += 1;
                toks.push(Tok::Str(s));
            }
            '\'' => {
                let start = i + 1;
                let end = (start..chars.len())
                    .find(|&j| chars[j] == '\'')
                    .ok_or_else(|| ParseError::at("unterminated string", token_start))?;
                toks.push(Tok::Str(chars[start..end].iter().collect()));
                i = end + 1;
            }
            '$' if at(i + 1).is_some_and(is_ident) => {
                let start = i + 1;
                i = start;
                while at(i).is_some_and(is_ident) {
                    i += 1;
                }
                toks.push(Tok::Var(chars[start..i].iter().collect()));
                member_ok = true;
            }
            _ => {
                let start = i;
                while let Some(c) = at(i) {
                    // A colon ends a word only before whitespace, so `http://x` stays one word
                    // while `{name: string}` splits into key and colon.
                    let colon_end = c == ':' && at(i + 1).is_none_or(|n| n.is_whitespace());
                    if is_delimiter(c, nesting > 0) || colon_end {
                        break;
                    }
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                toks.push(match word.as_str() {
                    "&" => Tok::Amp,
                    w => redirect(w).unwrap_or(Tok::Word(word)),
                });
            }
        }
    }
    positions.resize(toks.len(), token_start);
    Ok((toks, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_and_methods() {
        assert_eq!(
            lex("$it.name $p.kill()").unwrap(),
            vec![
                Tok::Var("it".into()),
                Tok::Member("name".into()),
                Tok::Var("p".into()),
                Tok::Member("kill".into()),
                Tok::CallOpen,
                Tok::RParen
            ]
        );
    }

    #[test]
    fn record_keys() {
        assert_eq!(
            lex("{name: string} http://x").unwrap(),
            vec![
                Tok::LBrace,
                Tok::Word("name".into()),
                Tok::Colon,
                Tok::Word("string".into()),
                Tok::RBrace,
                Tok::Word("http://x".into())
            ]
        );
    }
}
