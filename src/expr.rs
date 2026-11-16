//! `{{#expr: ...}}` arithmetic/logic expression evaluator (ParserFunctions-style).

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Op(&'static str),
    Word(String),
}

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || (c == '.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let st = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == '.') {
                i += 1;
            }
            if i < b.len() && (b[i] == 'e' || b[i] == 'E') {
                let mut j = i + 1;
                if j < b.len() && (b[j] == '+' || b[j] == '-') {
                    j += 1;
                }
                if j < b.len() && b[j].is_ascii_digit() {
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    i = j;
                }
            }
            let t: String = b[st..i].iter().collect();
            out.push(Tok::Num(t.parse().map_err(|_| format!("Invalid number \"{t}\""))?));
        } else if c.is_alphabetic() {
            let st = i;
            while i < b.len() && b[i].is_alphabetic() {
                i += 1;
            }
            out.push(Tok::Word(b[st..i].iter().collect::<String>().to_lowercase()));
        } else {
            let two: String = b[i..(i + 2).min(b.len())].iter().collect();
            let op = match two.as_str() {
                "<=" => Some("<="),
                ">=" => Some(">="),
                "<>" | "!=" => Some("<>"),
                _ => None,
            };
            if let Some(op) = op {
                out.push(Tok::Op(op));
                i += 2;
                continue;
            }
            let op = match c {
                '+' => "+",
                '-' => "-",
                '*' => "*",
                '/' => "/",
                '^' => "^",
                '(' => "(",
                ')' => ")",
                '=' => "=",
                '<' => "<",
                '>' => ">",
                _ => return Err(format!("Unrecognized punctuation character \"{c}\"")),
            };
            out.push(Tok::Op(op));
            i += 1;
        }
    }
    Ok(out)
}

struct P {
    t: Vec<Tok>,
    i: usize,
    depth: usize,
}

const FUNCS: &[&str] = &["abs", "ceil", "floor", "trunc", "sqrt", "ln", "exp", "sin", "cos", "tan", "asin", "acos", "atan", "sign"];

fn b2f(b: bool) -> f64 {
    if b { 1.0 } else { 0.0 }
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(x)) if x == w)
    }
    fn is_op(&self, o: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(x)) if *x == o)
    }
    fn or(&mut self) -> Result<f64, String> {
        let mut l = self.and()?;
        while self.is_word("or") {
            self.i += 1;
            let r = self.and()?;
            l = b2f(l != 0.0 || r != 0.0);
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<f64, String> {
        let mut l = self.cmp()?;
        while self.is_word("and") {
            self.i += 1;
            let r = self.cmp()?;
            l = b2f(l != 0.0 && r != 0.0);
        }
        Ok(l)
    }
    fn cmp(&mut self) -> Result<f64, String> {
        let l = self.add()?;
        if let Some(Tok::Op(op)) = self.peek().cloned() {
            if matches!(op, "=" | "<>" | "<" | ">" | "<=" | ">=") {
                self.i += 1;
                let r = self.add()?;
                return Ok(b2f(match op {
                    "=" => l == r,
                    "<>" => l != r,
                    "<" => l < r,
                    ">" => l > r,
                    "<=" => l <= r,
                    _ => l >= r,
                }));
            }
        }
        Ok(l)
    }
    fn add(&mut self) -> Result<f64, String> {
        let mut l = self.mul()?;
        loop {
            if self.is_op("+") {
                self.i += 1;
                l += self.mul()?;
            } else if self.is_op("-") {
                self.i += 1;
                l -= self.mul()?;
            } else if self.is_word("round") {
                self.i += 1;
                let n = self.mul()?;
                let f = 10f64.powi(n as i32);
                l = (l * f).round() / f;
            } else {
                return Ok(l);
            }
        }
    }
    fn mul(&mut self) -> Result<f64, String> {
        let mut l = self.pow()?;
        loop {
            if self.is_op("*") {
                self.i += 1;
                l *= self.pow()?;
            } else if self.is_op("/") || self.is_word("div") {
                self.i += 1;
                let r = self.pow()?;
                if r == 0.0 {
                    return Err("Division by zero".into());
                }
                l /= r;
            } else if self.is_word("fmod") {
                self.i += 1;
                let r = self.pow()?;
                if r == 0.0 {
                    return Err("Division by zero".into());
                }
                l %= r;
            } else if self.is_word("mod") {
                self.i += 1;
                let r = self.pow()?.trunc();
                if r == 0.0 {
                    return Err("Division by zero".into());
                }
                l = l.trunc() % r;
            } else {
                return Ok(l);
            }
        }
    }
    fn pow(&mut self) -> Result<f64, String> {
        let mut l = self.unary()?;
        while self.is_op("^") {
            self.i += 1;
            l = l.powf(self.unary()?);
        }
        Ok(l)
    }
    fn unary(&mut self) -> Result<f64, String> {
        self.depth += 1;
        if self.depth > 64 {
            return Err("Expression too deeply nested".into());
        }
        let r = self.unary_inner();
        self.depth -= 1;
        r
    }
    fn unary_inner(&mut self) -> Result<f64, String> {
        match self.peek().cloned() {
            Some(Tok::Op("-")) => {
                self.i += 1;
                Ok(-self.unary()?)
            }
            Some(Tok::Op("+")) => {
                self.i += 1;
                self.unary()
            }
            Some(Tok::Word(w)) if w == "not" => {
                self.i += 1;
                Ok(b2f(self.unary()? == 0.0))
            }
            Some(Tok::Word(w)) if FUNCS.contains(&w.as_str()) => {
                self.i += 1;
                let x = self.unary()?;
                Ok(match w.as_str() {
                    "abs" => x.abs(),
                    "ceil" => x.ceil(),
                    "floor" => x.floor(),
                    "trunc" => x.trunc(),
                    "sqrt" if x < 0.0 => return Err("Invalid argument for sqrt".into()),
                    "sqrt" => x.sqrt(),
                    "ln" if x <= 0.0 => return Err("Invalid argument for ln".into()),
                    "ln" => x.ln(),
                    "exp" => x.exp(),
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    "tan" => x.tan(),
                    "asin" => x.asin(),
                    "acos" => x.acos(),
                    "atan" => x.atan(),
                    _ => if x == 0.0 { 0.0 } else { x.signum() },
                })
            }
            Some(Tok::Num(n)) => {
                self.i += 1;
                Ok(n)
            }
            Some(Tok::Word(w)) if w == "pi" => {
                self.i += 1;
                Ok(std::f64::consts::PI)
            }
            Some(Tok::Word(w)) if w == "e" => {
                self.i += 1;
                Ok(std::f64::consts::E)
            }
            Some(Tok::Op("(")) => {
                self.i += 1;
                let v = self.or()?;
                if !self.is_op(")") {
                    return Err("Missing closing parenthesis".into());
                }
                self.i += 1;
                Ok(v)
            }
            Some(Tok::Word(w)) => Err(format!("Unrecognized word \"{w}\"")),
            Some(Tok::Op(o)) => Err(format!("Unexpected \"{o}\" operator")),
            None => Err("Unexpected end of expression".into()),
        }
    }
}

pub fn eval(s: &str) -> Result<f64, String> {
    if s.trim().is_empty() {
        return Ok(0.0);
    }
    let mut p = P { t: lex(s)?, i: 0, depth: 0 };
    let v = p.or()?;
    if p.i < p.t.len() {
        return Err("Unexpected token".into());
    }
    if v.is_nan() || v.is_infinite() {
        return Err("Result is not a number".into());
    }
    Ok(v)
}

pub fn fmt(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    let s = format!("{x:.14}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" { "0".into() } else { s.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(s: &str) -> String {
        match eval(s) {
            Ok(v) => fmt(v),
            Err(m) => format!("ERR {m}"),
        }
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(e("1+2*3"), "7");
        assert_eq!(e("(1+2)*3"), "9");
        assert_eq!(e("-3 + 5"), "2");
        assert_eq!(e("7 mod 3"), "1");
        assert_eq!(e("0.1+0.3"), "0.4");
        assert_eq!(e("1/3 round 3"), "0.333");
        assert_eq!(e("1e3 + 1"), "1001");
    }

    #[test]
    fn logic_and_functions() {
        assert_eq!(e("1 < 2 and 3 >= 3"), "1");
        assert_eq!(e("5 <> 5"), "0");
        assert_eq!(e("sqrt 16 + abs -2 + ceil 1.2 + floor 1.9"), "9");
        assert_eq!(e("pi round 2"), "3.14");
    }

    #[test]
    fn errors() {
        assert!(e("1/0").starts_with("ERR Division"));
        assert!(e("1 +").starts_with("ERR"));
        assert!(e("foo").starts_with("ERR Unrecognized word"));
        assert!(e("2 $ 3").starts_with("ERR"));
        assert!(e(&"(".repeat(500)).starts_with("ERR"));
    }
}
