// SPDX license expressions as crates write them in Cargo.toml: AND, OR,
// WITH, parentheses, and the old "MIT/Apache-2.0" form. The result is every
// way to satisfy the expression, each a set of licenses that apply together.

pub fn alternatives(expression: &str) -> Result<Vec<Vec<String>>, String> {
    let tokens = tokens(expression);
    if tokens.is_empty() {
        return Err(format!("the license expression {expression:?} is empty"));
    }
    let mut parser = Parser {
        tokens: &tokens,
        at: 0,
        expression,
    };
    let result = parser.or()?;
    if parser.at != tokens.len() {
        return Err(format!(
            "the license expression {expression:?} has {:?} where it should end",
            tokens[parser.at]
        ));
    }
    Ok(result)
}

fn tokens(expression: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in expression.chars() {
        if c.is_whitespace() || matches!(c, '(' | ')' | '/') {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            match c {
                '/' => out.push("OR".to_string()),
                '(' | ')' => out.push(c.to_string()),
                _ => {}
            }
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

struct Parser<'a> {
    tokens: &'a [String],
    at: usize,
    expression: &'a str,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.at).map(String::as_str)
    }

    fn or(&mut self) -> Result<Vec<Vec<String>>, String> {
        let mut result = self.and()?;
        while self.peek().is_some_and(|t| t.eq_ignore_ascii_case("OR")) {
            self.at += 1;
            for alternative in self.and()? {
                if !result.contains(&alternative) {
                    result.push(alternative);
                }
            }
        }
        Ok(result)
    }

    fn and(&mut self) -> Result<Vec<Vec<String>>, String> {
        let mut result = self.term()?;
        while self.peek().is_some_and(|t| t.eq_ignore_ascii_case("AND")) {
            self.at += 1;
            let right = self.term()?;
            let mut combined = Vec::new();
            for left in &result {
                for more in &right {
                    let mut both = left.clone();
                    for license in more {
                        if !both.contains(license) {
                            both.push(license.clone());
                        }
                    }
                    if !combined.contains(&both) {
                        combined.push(both);
                    }
                }
            }
            result = combined;
        }
        Ok(result)
    }

    fn term(&mut self) -> Result<Vec<Vec<String>>, String> {
        let expression = self.expression;
        match self.peek() {
            None => Err(format!(
                "the license expression {expression:?} ends where a license should be"
            )),
            Some("(") => {
                self.at += 1;
                let inner = self.or()?;
                if self.peek() != Some(")") {
                    return Err(format!(
                        "the license expression {expression:?} has an unclosed parenthesis"
                    ));
                }
                self.at += 1;
                Ok(inner)
            }
            Some(word) if is_operator(word) => Err(format!(
                "the license expression {expression:?} has {word} where a license should be"
            )),
            Some(word) => {
                let mut license = word.to_string();
                self.at += 1;
                if self.peek().is_some_and(|t| t.eq_ignore_ascii_case("WITH")) {
                    self.at += 1;
                    match self.peek() {
                        Some(exception) if !is_operator(exception) => {
                            license = format!("{license} WITH {exception}");
                            self.at += 1;
                        }
                        _ => {
                            return Err(format!(
                                "the license expression {expression:?} has WITH and no exception"
                            ));
                        }
                    }
                }
                Ok(vec![vec![license]])
            }
        }
    }
}

fn is_operator(word: &str) -> bool {
    ["AND", "OR", "WITH", "(", ")"]
        .iter()
        .any(|op| word.eq_ignore_ascii_case(op))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(expression: &str, expected: &[&[&str]]) {
        let expected: Vec<Vec<String>> = expected
            .iter()
            .map(|set| set.iter().map(|l| l.to_string()).collect())
            .collect();
        assert_eq!(alternatives(expression).unwrap(), expected, "{expression}");
    }

    #[test]
    fn reads_the_forms_in_booths_lock() {
        check("MIT OR Apache-2.0", &[&["MIT"], &["Apache-2.0"]]);
        check("MIT/Apache-2.0", &[&["MIT"], &["Apache-2.0"]]);
        check("Apache-2.0 / MIT", &[&["Apache-2.0"], &["MIT"]]);
        check("BSL-1.0", &[&["BSL-1.0"]]);
        check("MIT AND BSD-3-Clause", &[&["MIT", "BSD-3-Clause"]]);
        check(
            "(MIT OR Apache-2.0) AND Unicode-3.0",
            &[&["MIT", "Unicode-3.0"], &["Apache-2.0", "Unicode-3.0"]],
        );
        check(
            "Apache-2.0 OR GPL-2.0-only",
            &[&["Apache-2.0"], &["GPL-2.0-only"]],
        );
        check(
            "0BSD OR MIT OR Apache-2.0",
            &[&["0BSD"], &["MIT"], &["Apache-2.0"]],
        );
    }

    #[test]
    fn keeps_exceptions_with_their_license() {
        check(
            "Apache-2.0 WITH LLVM-exception OR MIT",
            &[&["Apache-2.0 WITH LLVM-exception"], &["MIT"]],
        );
    }

    #[test]
    fn and_binds_tighter_than_or() {
        check(
            "MIT OR Apache-2.0 AND Zlib",
            &[&["MIT"], &["Apache-2.0", "Zlib"]],
        );
        check("MIT AND MIT", &[&["MIT"]]);
    }

    #[test]
    fn refuses_what_does_not_parse() {
        for bad in [
            "",
            "MIT OR",
            "(MIT",
            "MIT)",
            "AND MIT",
            "MIT WITH",
            "MIT OR OR Zlib",
            "MIT Zlib",
        ] {
            assert!(alternatives(bad).is_err(), "{bad:?}");
        }
    }
}
