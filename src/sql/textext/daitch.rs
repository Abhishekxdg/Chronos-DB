//! `daitch_mokotoff(text)`: Daitch-Mokotoff soundex as contrib/fuzzystrmatch computes it. The
//! coding chart is the one at jewishgen.org/InfoFiles/Soundex.html (with the same unofficial rules
//! for "UE" as every other implementation). A name is read as letter sequences (`SCHT`, `CH`, `S`, ...)
//! and each has up to two codes, each with a digit string for the start of the name, before a vowel
//! and anywhere else. The codes are grown a digit at a time in a tree whose leaves are the codes so
//! far; a letter with two codings forks them, so a name can have several six-digit codes.

/// One letter sequence's codes: up to two alternatives, each the digits for the start of the name,
/// before a vowel, and any other place (`X` is not coded). An unused second alternative is all
/// empty strings.
type Codes = [[&'static str; 3]; 2];

/// The coding chart. A letter sequence's entry is found by its longest match in the name, so the
/// order does not matter; `[`, `\` and `]` stand for A, E and T with an ogonek or a cedilla (see
/// `read_char`).
#[rustfmt::skip]
const CHART: &[(&str, Codes)] = &[
    ("AI", [["0", "1", "X"], [""; 3]]),
    ("AJ", [["0", "1", "X"], [""; 3]]),
    ("AY", [["0", "1", "X"], [""; 3]]),
    ("AU", [["0", "7", "X"], [""; 3]]),
    ("[", [["X", "X", "6"], ["X", "X", "X"]]),
    ("A", [["0", "X", "X"], [""; 3]]),
    ("B", [["7", "7", "7"], [""; 3]]),
    ("CHS", [["5", "54", "54"], [""; 3]]),
    ("CH", [["5", "5", "5"], ["4", "4", "4"]]),
    ("CK", [["5", "5", "5"], ["45", "45", "45"]]),
    ("CZ", [["4", "4", "4"], [""; 3]]),
    ("CS", [["4", "4", "4"], [""; 3]]),
    ("CSZ", [["4", "4", "4"], [""; 3]]),
    ("CZS", [["4", "4", "4"], [""; 3]]),
    ("C", [["5", "5", "5"], ["4", "4", "4"]]),
    ("DRZ", [["4", "4", "4"], [""; 3]]),
    ("DRS", [["4", "4", "4"], [""; 3]]),
    ("DS", [["4", "4", "4"], [""; 3]]),
    ("DSH", [["4", "4", "4"], [""; 3]]),
    ("DSZ", [["4", "4", "4"], [""; 3]]),
    ("DZ", [["4", "4", "4"], [""; 3]]),
    ("DZH", [["4", "4", "4"], [""; 3]]),
    ("DZS", [["4", "4", "4"], [""; 3]]),
    ("D", [["3", "3", "3"], [""; 3]]),
    ("DT", [["3", "3", "3"], [""; 3]]),
    ("EI", [["0", "1", "X"], [""; 3]]),
    ("EJ", [["0", "1", "X"], [""; 3]]),
    ("EY", [["0", "1", "X"], [""; 3]]),
    ("EU", [["1", "1", "X"], [""; 3]]),
    ("\\", [["X", "X", "6"], ["X", "X", "X"]]),
    ("E", [["0", "X", "X"], [""; 3]]),
    ("FB", [["7", "7", "7"], [""; 3]]),
    ("F", [["7", "7", "7"], [""; 3]]),
    ("G", [["5", "5", "5"], [""; 3]]),
    ("H", [["5", "5", "X"], [""; 3]]),
    ("IA", [["1", "X", "X"], [""; 3]]),
    ("IE", [["1", "X", "X"], [""; 3]]),
    ("IO", [["1", "X", "X"], [""; 3]]),
    ("IU", [["1", "X", "X"], [""; 3]]),
    ("I", [["0", "X", "X"], [""; 3]]),
    ("J", [["1", "X", "X"], ["4", "4", "4"]]),
    ("KS", [["5", "54", "54"], [""; 3]]),
    ("KH", [["5", "5", "5"], [""; 3]]),
    ("K", [["5", "5", "5"], [""; 3]]),
    ("L", [["8", "8", "8"], [""; 3]]),
    ("MN", [["66", "66", "66"], [""; 3]]),
    ("M", [["6", "6", "6"], [""; 3]]),
    ("NM", [["66", "66", "66"], [""; 3]]),
    ("N", [["6", "6", "6"], [""; 3]]),
    ("OI", [["0", "1", "X"], [""; 3]]),
    ("OJ", [["0", "1", "X"], [""; 3]]),
    ("OY", [["0", "1", "X"], [""; 3]]),
    ("O", [["0", "X", "X"], [""; 3]]),
    ("P", [["7", "7", "7"], [""; 3]]),
    ("PF", [["7", "7", "7"], [""; 3]]),
    ("PH", [["7", "7", "7"], [""; 3]]),
    ("Q", [["5", "5", "5"], [""; 3]]),
    ("RZ", [["94", "94", "94"], ["4", "4", "4"]]),
    ("RS", [["94", "94", "94"], ["4", "4", "4"]]),
    ("R", [["9", "9", "9"], [""; 3]]),
    ("SCHTSCH", [["2", "4", "4"], [""; 3]]),
    ("SCHTSH", [["2", "4", "4"], [""; 3]]),
    ("SCHTCH", [["2", "4", "4"], [""; 3]]),
    ("SCH", [["4", "4", "4"], [""; 3]]),
    ("SHTCH", [["2", "4", "4"], [""; 3]]),
    ("SHCH", [["2", "4", "4"], [""; 3]]),
    ("SHTSH", [["2", "4", "4"], [""; 3]]),
    ("SHT", [["2", "43", "43"], [""; 3]]),
    ("SCHT", [["2", "43", "43"], [""; 3]]),
    ("SCHD", [["2", "43", "43"], [""; 3]]),
    ("SH", [["4", "4", "4"], [""; 3]]),
    ("STCH", [["2", "4", "4"], [""; 3]]),
    ("STSCH", [["2", "4", "4"], [""; 3]]),
    ("SC", [["2", "4", "4"], [""; 3]]),
    ("STRZ", [["2", "4", "4"], [""; 3]]),
    ("STRS", [["2", "4", "4"], [""; 3]]),
    ("STSH", [["2", "4", "4"], [""; 3]]),
    ("ST", [["2", "43", "43"], [""; 3]]),
    ("SZCZ", [["2", "4", "4"], [""; 3]]),
    ("SZCS", [["2", "4", "4"], [""; 3]]),
    ("SZT", [["2", "43", "43"], [""; 3]]),
    ("SHD", [["2", "43", "43"], [""; 3]]),
    ("SZD", [["2", "43", "43"], [""; 3]]),
    ("SD", [["2", "43", "43"], [""; 3]]),
    ("SZ", [["4", "4", "4"], [""; 3]]),
    ("S", [["4", "4", "4"], [""; 3]]),
    ("TCH", [["4", "4", "4"], [""; 3]]),
    ("TTCH", [["4", "4", "4"], [""; 3]]),
    ("TTSCH", [["4", "4", "4"], [""; 3]]),
    ("TH", [["3", "3", "3"], [""; 3]]),
    ("TRZ", [["4", "4", "4"], [""; 3]]),
    ("TRS", [["4", "4", "4"], [""; 3]]),
    ("TSCH", [["4", "4", "4"], [""; 3]]),
    ("TSH", [["4", "4", "4"], [""; 3]]),
    ("TS", [["4", "4", "4"], [""; 3]]),
    ("TTS", [["4", "4", "4"], [""; 3]]),
    ("TTSZ", [["4", "4", "4"], [""; 3]]),
    ("TC", [["4", "4", "4"], [""; 3]]),
    ("TZ", [["4", "4", "4"], [""; 3]]),
    ("TTZ", [["4", "4", "4"], [""; 3]]),
    ("TZS", [["4", "4", "4"], [""; 3]]),
    ("TSZ", [["4", "4", "4"], [""; 3]]),
    ("]", [["3", "3", "3"], ["4", "4", "4"]]),
    ("T", [["3", "3", "3"], [""; 3]]),
    ("UI", [["0", "1", "X"], [""; 3]]),
    ("UJ", [["0", "1", "X"], [""; 3]]),
    ("UY", [["0", "1", "X"], [""; 3]]),
    ("UE", [["0", "1", "X"], [""; 3]]),
    ("U", [["0", "X", "X"], [""; 3]]),
    ("V", [["7", "7", "7"], [""; 3]]),
    ("W", [["7", "7", "7"], [""; 3]]),
    ("X", [["5", "54", "54"], [""; 3]]),
    ("Y", [["1", "X", "X"], [""; 3]]),
    ("ZDZ", [["2", "4", "4"], [""; 3]]),
    ("ZDZH", [["2", "4", "4"], [""; 3]]),
    ("ZHDZH", [["2", "4", "4"], [""; 3]]),
    ("ZD", [["2", "43", "43"], [""; 3]]),
    ("ZHD", [["2", "43", "43"], [""; 3]]),
    ("ZH", [["4", "4", "4"], [""; 3]]),
    ("ZS", [["4", "4", "4"], [""; 3]]),
    ("ZSCH", [["4", "4", "4"], [""; 3]]),
    ("ZSH", [["4", "4", "4"], [""; 3]]),
    ("Z", [["4", "4", "4"], [""; 3]]),
];

/// What follows the last letter: a place where nothing is coded.
const END_CODES: Codes = [["X", "X", "X"], [""; 3]];

/// ISO 8859-1 (0x60 to 0xFF) as upper-case ASCII for the chart, with a blank where there is no letter.
const LATIN1_UPPER: &[u8; 160] = b"`ABCDEFGHIJKLMNOPQRSTUVWXYZ{|}~                                  !                             ?AAAAAAECEEEEIIIIDNOOOOO*OUUUUYDSAAAAAAECEEEEIIIIDNOOOOO/OUUUUYDY";

/// Placeholder for a character that is not in the chart.
const NA: u8 = 0x1a;

/// Six digits make a code.
const DIGITS: usize = 6;

/// The next character of the name as the ASCII the chart uses (0 at the end).
fn read_char(word: &[char], ix: &mut usize) -> u8 {
    let Some(&ch) = word.get(*ix) else {
        return 0;
    };
    *ix += 1;
    let c = ch as u32;
    if (u32::from(b'[')..=u32::from(b']')).contains(&c) {
        // these three ASCII characters are reserved for the conversions below
        NA
    } else if c < 0x60 {
        c as u8
    } else if c < 0x100 {
        LATIN1_UPPER[(c - 0x60) as usize]
    } else {
        match c {
            0x0104 | 0x0105 => b'[',
            0x0118 | 0x0119 => b'\\',
            0x0162 | 0x0163 | 0x021A | 0x021B => b']',
            _ => NA,
        }
    }
}

/// The next character the chart has a use for (`A` to `]`), or `None` at the end.
fn read_valid_char(word: &[char], ix: &mut usize) -> Option<u8> {
    loop {
        match read_char(word, ix) {
            0 => return None,
            c @ b'A'..=b']' => return Some(c),
            _ => {}
        }
    }
}

/// The codes of the longest letter sequence the name has next, moving `ix` past it. Characters the
/// chart ignores are skipped between the letters of a sequence. `None` at the end of the name.
fn read_letter(word: &[char], ix: &mut usize) -> Option<&'static Codes> {
    let first = read_valid_char(word, ix)?;
    // the letters after it, as many as the longest sequence in the chart, and where each one ends
    let mut seq = vec![first];
    let mut ends = vec![*ix];
    let mut i = *ix;
    while seq.len() < 7 {
        let Some(c) = read_valid_char(word, &mut i) else { break };
        seq.push(c);
        ends.push(i);
    }
    for len in (1..=seq.len()).rev() {
        if let Some((_, codes)) = CHART.iter().find(|(key, _)| key.as_bytes() == &seq[..len]) {
            *ix = ends[len - 1];
            return Some(codes);
        }
    }
    None
}

/// A node of the tree of codes: the digits so far, and what is needed to extend them with the next
/// letter's digits.
struct Node {
    length: usize,
    soundex: [u8; DIGITS],
    is_leaf: bool,
    /// the number of the letter that last changed the node
    last_update: i32,
    /// the digit or digits (two, one of them `X`, at a fork) that led here from the letter before, and
    /// those being added for this letter
    prev_code_digits: [u8; 2],
    next_code_digits: [u8; 2],
    /// which codings (1 before a vowel, 2 any other) led here, ORed
    prev_code_index: u8,
    next_code_index: u8,
    children: [Option<usize>; 10],
    /// the next leaf in the list for each of the two alternating passes
    next: [Option<usize>; 2],
}

impl Node {
    fn start() -> Node {
        Node {
            length: 0,
            soundex: *b"000000",
            is_leaf: false,
            last_update: 0,
            prev_code_digits: [0; 2],
            next_code_digits: [0; 2],
            prev_code_index: 0,
            next_code_index: 0,
            children: [None; 10],
            next: [None; 2],
        }
    }
}

/// Where a pass is: the nodes, the codes that got all six digits (in the order they did), and the
/// first and last leaf of the list being built for each of the two alternating passes.
struct Tree {
    nodes: Vec<Node>,
    done: Vec<String>,
    first: [Option<usize>; 2],
    last: [Option<usize>; 2],
}

impl Tree {
    /// Gets a node ready for the next letter's digit, unless that has been done already.
    fn initialize_node(&mut self, n: usize, last_update: i32) {
        let node = &mut self.nodes[n];
        if node.last_update < last_update {
            node.prev_code_digits = node.next_code_digits;
            node.next_code_digits = [0; 2];
            node.prev_code_index = node.next_code_index;
            node.next_code_index = 0;
            node.is_leaf = false;
            node.last_update = last_update;
        }
    }

    /// Puts a node on the list of leaves for a pass, if it is not there.
    fn set_leaf(&mut self, n: usize, ix: usize) {
        if !self.nodes[n].is_leaf {
            self.nodes[n].is_leaf = true;
            // a list being built has a first leaf exactly when it has a last one
            match self.last[ix] {
                Some(last) => self.nodes[last].next[ix] = Some(n),
                None => self.first[ix] = Some(n),
            }
            self.last[ix] = Some(n);
            self.nodes[n].next[ix] = None;
        }
    }

    /// The child for a digit, made if need be: a new one that reaches six digits is a finished code
    /// and gives `None`, as does one that has been finished already.
    fn find_or_create_child(&mut self, parent: usize, digit: u8) -> Option<usize> {
        let i = usize::from(digit - b'0');
        if let Some(child) = self.nodes[parent].children[i] {
            return (self.nodes[child].length < DIGITS).then_some(child);
        }
        let mut node = Node::start();
        node.soundex = self.nodes[parent].soundex;
        node.length = self.nodes[parent].length;
        node.soundex[node.length] = digit;
        node.length += 1;
        node.next_code_index = node.prev_code_index;
        let child = self.nodes.len();
        let complete = node.length >= DIGITS;
        if complete {
            self.done.push(String::from_utf8_lossy(&node.soundex).into_owned());
        }
        self.nodes.push(node);
        self.nodes[parent].children[i] = Some(child);
        (!complete).then_some(child)
    }

    /// Extends a node with the digit at `digit_no` of a letter's code, and with the rest of the digits.
    #[allow(clippy::too_many_arguments)]
    fn update_node(
        &mut self,
        node: usize,
        ix: usize,
        letter_no: i32,
        prev_code_index: u8,
        next_code_index: u8,
        next_code_digits: &str,
        digit_no: usize,
    ) {
        let digits = next_code_digits.as_bytes();
        let digit_at = |k: usize| digits.get(k).copied().unwrap_or(0);
        let next_code_digit = digit_at(digit_no);
        self.initialize_node(node, letter_no);
        let n = &self.nodes[node];
        if n.prev_code_index != 0 && n.prev_code_index & prev_code_index == 0 {
            // the sound (vowel or consonant) of this letter's coding does not fit the one before; only
            // J is ever both
            return;
        }
        let mut dirty = [0usize; 2];
        let mut num_dirty = 0;
        if next_code_digit == b'X'
            || (digit_no == 0 && (n.prev_code_digits[0] == next_code_digit || n.prev_code_digits[1] == next_code_digit))
        {
            // the digit is the same as one before it, so not added
            dirty[num_dirty] = node;
            num_dirty += 1;
        }
        if next_code_digit != b'X'
            && (digit_no > 0 || n.prev_code_digits[0] != next_code_digit || n.prev_code_digits[1] != 0)
        {
            // the digit differs from one before it, so added
            if let Some(child) = self.find_or_create_child(node, next_code_digit) {
                self.initialize_node(child, letter_no);
                dirty[num_dirty] = child;
                num_dirty += 1;
            }
        }
        // note that `digit_no` moves on for every node here, as it does in Postgres
        let mut digit_no = digit_no;
        for &d in &dirty[..num_dirty] {
            let node = &mut self.nodes[d];
            node.next_code_index |= next_code_index;
            if node.next_code_digits[0] == 0 {
                node.next_code_digits[0] = next_code_digit;
            } else if node.next_code_digits[0] != next_code_digit {
                node.next_code_digits[1] = next_code_digit;
            }
            digit_no += 1;
            if digit_at(digit_no) != 0 {
                self.update_node(d, ix, letter_no, prev_code_index, next_code_index, next_code_digits, digit_no);
            } else {
                self.set_leaf(d, ix);
            }
        }
    }

    /// Takes every leaf one letter on: each coding of the letter against each coding of the next.
    /// `ix` is which of the two alternating leaf lists is the current one; the new one is built.
    fn update_leaves(&mut self, ix: &mut usize, letter_no: i32, codes: &Codes, next_codes: &Codes) {
        let next_ix = (*ix + 1) & 1;
        self.first[next_ix] = None;
        self.last[next_ix] = None;
        let mut node = self.first[*ix];
        while let Some(n) = node {
            for code in codes.iter().take_while(|c| !c[0].is_empty()) {
                // the coding of the letter before: before a vowel 1, all other 2
                let prev_code_index = u8::from(code[0].as_bytes()[0] > b'1') + 1;
                for next_code in next_codes.iter().take_while(|c| !c[0].is_empty()) {
                    let code_index = if letter_no == 0 {
                        0
                    } else if next_code[0].as_bytes()[0] <= b'1' {
                        // the next letter is a vowel
                        1
                    } else {
                        2
                    };
                    self.update_node(n, next_ix, letter_no, prev_code_index, code_index as u8, code[code_index], 0);
                }
            }
            node = self.nodes[n].next[*ix];
        }
        *ix = next_ix;
    }
}

/// `daitch_mokotoff(text)`: every code for the name (`daitch_mokotoff_coding`): the ones that got six
/// digits as they did, then the leaves that did not, padded with zeros. `None` if no character of the
/// name is coded.
pub(super) fn daitch_mokotoff(s: &str) -> Option<Vec<String>> {
    let word: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut codes = read_letter(&word, &mut i)?;
    let mut tree = Tree { nodes: vec![Node::start()], done: vec![], first: [Some(0), None], last: [None, None] };
    let (mut ix, mut letter_no) = (0, 0);
    // until the name runs out, or every code has six digits
    loop {
        let next_codes = read_letter(&word, &mut i);
        tree.update_leaves(&mut ix, letter_no, codes, next_codes.unwrap_or(&END_CODES));
        letter_no += 1;
        match next_codes {
            Some(c) if tree.first[ix].is_some() => codes = c,
            _ => break,
        }
    }
    let mut out = tree.done.clone();
    let mut node = tree.first[ix];
    while let Some(n) = node {
        out.push(String::from_utf8_lossy(&tree.nodes[n].soundex).into_owned());
        node = tree.nodes[n].next[ix];
    }
    Some(out)
}
