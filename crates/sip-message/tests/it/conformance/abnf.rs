//! An RFC 5234 grammar read from text, and a deterministic generator of the
//! values it defines. A string literal is case-insensitive by RFC 5234 §2.3, so
//! a generated literal sometimes comes out re-cased; `%s"..."` is exact.

use std::collections::BTreeMap;

/// One element of a rule.
#[derive(Debug)]
enum Node {
    Alt(Vec<Node>),
    Seq(Vec<Node>),
    Repeat {
        min: u32,
        max: Option<u32>,
        node: Box<Node>,
    },
    Rule(String),
    /// A string literal: `true` when its case may vary.
    Literal(Vec<u8>, bool),
    Range(u8, u8),
}

/// A seeded generator of small numbers: one sequence per seed, on any platform.
/// A steered one takes its choices from given bytes first, so a fuzzer that
/// mutates the bytes walks the grammar's derivations, each one valid.
pub struct Rng {
    state: u64,
    steer: Vec<u8>,
    at: usize,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed | 1, steer: Vec::new(), at: 0 }
    }

    /// A seed named by `text`.
    pub fn named(text: &str) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.bytes() {
            hash = (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self::new(hash)
    }

    /// The generator named by `text` whose first choices `steer` makes: a
    /// byte per choice of up to 256 outcomes, two per larger one.
    pub fn steered(text: &str, steer: &[u8]) -> Self {
        Self { steer: steer.to_vec(), ..Self::named(text) }
    }

    fn next(&mut self) -> u64 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    }

    /// The next steering byte, `None` once they are spent.
    fn steering(&mut self) -> Option<u64> {
        let byte = self.steer.get(self.at)?;
        self.at += 1;
        Some(u64::from(*byte))
    }

    fn choice(&mut self, n: usize) -> u64 {
        let steered = match self.steering() {
            Some(low) if n > 256 => self.steering().map(|high| high << 8 | low),
            steered => steered,
        };
        steered.unwrap_or_else(|| self.next())
    }

    pub fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.choice(n) % n as u64).expect("a small bound")
    }
}

/// How deep rules nest before the generator takes each rule's shortest form.
const DEPTH_LIMIT: u32 = 24;

/// Past its minimum, a repetition continues with this chance in eight.
const REPEAT_CONTINUE: usize = 3;

/// A literal comes out re-cased with one chance in this.
const RECASE_ONE_IN: usize = 8;

/// A parsed grammar: every rule it references is defined.
pub struct Grammar {
    rules: BTreeMap<String, Node>,
    floors: BTreeMap<String, u32>,
}

impl Grammar {
    pub fn parse(text: &str) -> Self {
        let mut rules: BTreeMap<String, Node> = BTreeMap::new();
        for statement in statements(text) {
            let (name, body) = statement.split_once('=').expect("a rule has an equals sign");
            let mut reader = Reader { text: body.as_bytes(), at: 0 };
            let node = reader.alternation();
            reader.skip_space();
            assert_eq!(reader.at, reader.text.len(), "the whole rule read: {statement}");
            let previous = rules.insert(name.trim().to_ascii_lowercase(), node);
            assert!(previous.is_none(), "one definition of {name}");
        }
        let mut grammar = Self { rules, floors: BTreeMap::new() };
        grammar.check_references();
        grammar.compute_floors();
        grammar
    }

    fn check_references(&self) {
        fn walk(node: &Node, rules: &BTreeMap<String, Node>) {
            match node {
                Node::Alt(list) | Node::Seq(list) => list.iter().for_each(|n| walk(n, rules)),
                Node::Repeat { node, .. } => walk(node, rules),
                Node::Rule(name) => assert!(rules.contains_key(name), "{name} is defined"),
                Node::Literal(..) | Node::Range(..) => {}
            }
        }
        self.rules.values().for_each(|node| walk(node, &self.rules));
    }

    /// The depth each rule's shortest form needs, by fixed point: a recursive
    /// rule's floor comes from its alternative that does not recurse.
    fn compute_floors(&mut self) {
        loop {
            let mut changed = false;
            for (name, node) in &self.rules {
                if let Some(depth) = floor_of(node, &self.floors) {
                    let depth = depth + 1;
                    if self.floors.get(name).is_none_or(|&known| depth < known) {
                        self.floors.insert(name.clone(), depth);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        for name in self.rules.keys() {
            assert!(self.floors.contains_key(name), "{name} has a finite form");
        }
    }

    /// One value of `rule`.
    pub fn generate(&self, rule: &str, rng: &mut Rng) -> Vec<u8> {
        let mut out = Vec::new();
        self.emit_rule(&rule.to_ascii_lowercase(), 0, rng, &mut out);
        out
    }

    fn emit_rule(&self, name: &str, depth: u32, rng: &mut Rng, out: &mut Vec<u8>) {
        let node = self.rules.get(name).unwrap_or_else(|| panic!("no rule {name}"));
        self.emit(node, depth + 1, rng, out);
    }

    fn emit(&self, node: &Node, depth: u32, rng: &mut Rng, out: &mut Vec<u8>) {
        match node {
            Node::Alt(list) => self.emit(self.choose(list, depth, rng), depth, rng, out),
            Node::Seq(list) => list.iter().for_each(|n| self.emit(n, depth, rng, out)),
            Node::Repeat { min, max, node } => {
                let mut count = *min;
                while depth < DEPTH_LIMIT
                    && max.is_none_or(|m| count < m)
                    && rng.below(8) < REPEAT_CONTINUE
                {
                    count += 1;
                }
                (0..count).for_each(|_| self.emit(node, depth, rng, out));
            }
            Node::Rule(name) => self.emit_rule(name, depth, rng, out),
            Node::Literal(bytes, may_recase) => {
                if *may_recase && rng.below(RECASE_ONE_IN) == 0 {
                    out.extend(bytes.iter().map(|b| {
                        if rng.below(2) == 0 {
                            b.to_ascii_uppercase()
                        } else {
                            b.to_ascii_lowercase()
                        }
                    }));
                } else {
                    out.extend_from_slice(bytes);
                }
            }
            Node::Range(low, high) => {
                let span = usize::from(*high) - usize::from(*low) + 1;
                out.push(*low + u8::try_from(rng.below(span)).expect("a byte range"));
            }
        }
    }

    /// Any alternative until the depth limit, then the shortest.
    fn choose<'a>(&self, list: &'a [Node], depth: u32, rng: &mut Rng) -> &'a Node {
        if depth < DEPTH_LIMIT {
            return &list[rng.below(list.len())];
        }
        list.iter()
            .min_by_key(|node| floor_of(node, &self.floors).unwrap_or(u32::MAX))
            .expect("an alternation has alternatives")
    }
}

/// The depth `node`'s shortest form needs, `None` while a rule it needs has no
/// floor yet.
fn floor_of(node: &Node, floors: &BTreeMap<String, u32>) -> Option<u32> {
    match node {
        Node::Alt(list) => list.iter().filter_map(|n| floor_of(n, floors)).min(),
        Node::Seq(list) => list
            .iter()
            .map(|n| floor_of(n, floors))
            .try_fold(0, |deepest, floor| floor.map(|floor| deepest.max(floor))),
        Node::Repeat { min: 0, .. } | Node::Literal(..) | Node::Range(..) => Some(0),
        Node::Repeat { node, .. } => floor_of(node, floors),
        Node::Rule(name) => floors.get(name).copied(),
    }
}

/// The rules of a grammar text: comments gone, continuation lines joined.
fn statements(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = strip_comment(line);
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            let last = out.last_mut().expect("a continuation follows a rule");
            last.push(' ');
            last.push_str(line.trim());
        } else {
            out.push(line.trim().to_string());
        }
    }
    out
}

/// `line` up to its comment; a `;` inside a string literal is no comment.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (at, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => return &line[..at],
            _ => {}
        }
    }
    line
}

/// A recursive-descent reader of one rule's right-hand side.
struct Reader<'a> {
    text: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(|b| b == b' ' || b == b'\t') {
            self.at += 1;
        }
    }

    fn alternation(&mut self) -> Node {
        let mut list = vec![self.concatenation()];
        loop {
            self.skip_space();
            if self.peek() != Some(b'/') {
                break;
            }
            self.at += 1;
            list.push(self.concatenation());
        }
        if list.len() == 1 {
            list.pop().expect("one alternative")
        } else {
            Node::Alt(list)
        }
    }

    fn concatenation(&mut self) -> Node {
        let mut list = Vec::new();
        loop {
            self.skip_space();
            match self.peek() {
                None | Some(b'/' | b')' | b']') => break,
                Some(_) => list.push(self.repetition()),
            }
        }
        assert!(!list.is_empty(), "an empty concatenation at {}", self.at);
        if list.len() == 1 {
            list.pop().expect("one element")
        } else {
            Node::Seq(list)
        }
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.at += 1;
        }
        std::str::from_utf8(&self.text[start..self.at]).ok()?.parse().ok()
    }

    fn repetition(&mut self) -> Node {
        let low = self.number();
        let (min, max) = if self.peek() == Some(b'*') {
            self.at += 1;
            (low.unwrap_or(0), self.number())
        } else {
            (low.unwrap_or(1), low.or(Some(1)))
        };
        let node = self.element();
        if (min, max) == (1, Some(1)) {
            node
        } else {
            Node::Repeat { min, max, node: Box::new(node) }
        }
    }

    fn element(&mut self) -> Node {
        match self.peek().expect("an element") {
            b'(' => {
                self.at += 1;
                let node = self.alternation();
                self.closing(b')');
                node
            }
            b'[' => {
                self.at += 1;
                let node = self.alternation();
                self.closing(b']');
                Node::Repeat { min: 0, max: Some(1), node: Box::new(node) }
            }
            b'"' => self.literal(true),
            b'%' => self.numeric(),
            _ => self.rule_name(),
        }
    }

    fn closing(&mut self, close: u8) {
        self.skip_space();
        assert_eq!(self.peek(), Some(close), "a closing bracket at {}", self.at);
        self.at += 1;
    }

    fn literal(&mut self, may_recase: bool) -> Node {
        self.at += 1;
        let start = self.at;
        while self.peek().expect("a closing quote") != b'"' {
            self.at += 1;
        }
        let bytes = self.text[start..self.at].to_vec();
        self.at += 1;
        Node::Literal(bytes, may_recase)
    }

    fn numeric(&mut self) -> Node {
        self.at += 1;
        let base = self.peek().expect("a base letter");
        self.at += 1;
        if base == b's' {
            return self.literal(false);
        }
        assert_eq!(base, b'x', "only hexadecimal values are used");
        let first = self.hex();
        match self.peek() {
            Some(b'-') => {
                self.at += 1;
                Node::Range(first, self.hex())
            }
            Some(b'.') => {
                let mut bytes = vec![first];
                while self.peek() == Some(b'.') {
                    self.at += 1;
                    bytes.push(self.hex());
                }
                Node::Literal(bytes, false)
            }
            _ => Node::Literal(vec![first], false),
        }
    }

    fn hex(&mut self) -> u8 {
        let start = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_hexdigit()) {
            self.at += 1;
        }
        let digits = std::str::from_utf8(&self.text[start..self.at]).expect("ascii");
        u8::from_str_radix(digits, 16).expect("a byte value")
    }

    fn rule_name(&mut self) -> Node {
        let start = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'-') {
            self.at += 1;
        }
        let name = std::str::from_utf8(&self.text[start..self.at]).expect("ascii");
        assert!(!name.is_empty(), "a rule name at {start}");
        Node::Rule(name.to_ascii_lowercase())
    }
}
