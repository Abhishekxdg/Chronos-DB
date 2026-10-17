//! Postgres 16's network address types as plain values: `inet`, `cidr`, `macaddr` and `macaddr8`.
//! Input and output text, ordering, containment, bit operations, arithmetic and the conversions
//! between the types all follow Postgres, errors and their SQLSTATEs included. Checked against a
//! real Postgres 16 (see the `pg_diff_file` test at the bottom).
//!
//! The parsers are ports of the C ones (`inet_net_pton.c`, `mac.c`, `mac8.c`), quirks and all:
//! leading zeros are fine in `10.01.1.1` but not inside `::1.02.3.4`; `macaddr` is whatever glibc's
//! `sscanf("%x")` accepts (`0x1: 2:+3:4:5:6`); `abbrev(cidr)` prints `2001:db8:0:1::/64` as
//! `2001:db8::1/64`. Nothing here is wired into SQL yet.
//!
//! Type resolution is here too, since what Postgres says about a call no signature fits depends on
//! the network types' implicit casts: `cidr` goes to `inet` for free, `macaddr` and `macaddr8` go to
//! each other both ways (so `macaddr = macaddr8` is 42725, not unique, rather than 42883), and an
//! `inet` never becomes a mac. [`resolve`] and [`compare_with`] pick an operator by its operand
//! types, every other function checks its own arguments, and all of them word the failure as
//! Postgres does: `operator does not exist: macaddr & inet`, `function set_masklen(macaddr, integer)
//! does not exist`, `cannot cast type macaddr to inet`.
//!
//! Stored form: `encode` gives bytes that sort (memcmp) exactly like `Ord`, so a B-tree can use them
//! as its key; `decode` is its inverse.
//! ponytail: `inet_client_addr()` and friends, the `inet`/`cidr` aggregates, and HINT lines on
//! errors (`SqlError` has no field for them; Postgres gives one to `operator does not exist`,
//! `function ... does not exist`, `operator is not unique` and macaddr8 -> macaddr) are not here. Nor
//! are calls with an untyped literal or NULL operand, which Postgres resolves by other rules.

use super::{BAD_INPUT, NO_FUNCTION, RANGE, Result, SqlError, Type, Violation, err, no_op};
use std::cmp::Ordering;
use std::fmt;

/// 22023, `invalid_parameter_value`: a mask length or a pair of addresses Postgres won't take.
const BAD_ARG: &str = "22023";
/// 42725, `ambiguous_function`: two operators fit equally well, so Postgres won't pick.
const AMBIGUOUS: &str = "42725";
/// 42846, `cannot_coerce`: there is no cast between the two types.
const CANNOT_COERCE: &str = "42846";
/// 22P03, `invalid_binary_representation`: a value in the wire's binary form that isn't one.
const INVALID_BINARY: &str = "22P03";
/// 08P01, `protocol_violation`: a binary value that ends before it should.
const PROTOCOL: &str = "08P01";

/// An `inet` or `cidr` value: an address with a netmask length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Addr {
    pub v6: bool,
    /// 0..=32 for IPv4, 0..=128 for IPv6.
    pub bits: u8,
    /// The address; an IPv4 one lives in the low 32 bits.
    pub ip: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Net {
    Inet(Addr),
    /// An address whose bits to the right of the mask are all zero.
    Cidr(Addr),
    /// 48 bits.
    Mac(u64),
    Mac8(u64),
}

/// Which type a text is read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    Inet,
    Cidr,
    Mac,
    Mac8,
}

impl Kind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Kind::Inet => "inet",
            Kind::Cidr => "cidr",
            Kind::Mac => "macaddr",
            Kind::Mac8 => "macaddr8",
        }
    }
}

/// The SQL type of an operand, for choosing an operator or function and for naming the types when
/// none fits. The integers are here only because `inet + bigint` is an operator and `macaddr +
/// integer` has to be reported with the integer's own name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Ty {
    Net(Kind),
    /// (a smallint column is an integer to the SQL layer, which can't tell them apart: only the tests name this)
    #[cfg_attr(not(test), allow(dead_code))]
    Int2,
    Int4,
    Int8,
}

impl Ty {
    /// The name Postgres prints in its messages.
    pub(super) fn name(self) -> &'static str {
        match self {
            Ty::Net(k) => k.name(),
            Ty::Int2 => "smallint",
            Ty::Int4 => "integer",
            Ty::Int8 => "bigint",
        }
    }

    /// An `inet` or a `cidr`, which Postgres treats alike wherever an operator wants an `inet`.
    fn is_ip(self) -> bool {
        matches!(self, Ty::Net(Kind::Inet | Kind::Cidr))
    }

    /// Any integer type: each one converts to `bigint` implicitly.
    fn is_int(self) -> bool {
        matches!(self, Ty::Int2 | Ty::Int4 | Ty::Int8)
    }
}

impl Net {
    pub(super) fn ty(&self) -> Ty {
        Ty::Net(self.kind())
    }

    pub(super) fn kind(&self) -> Kind {
        match self {
            Net::Inet(_) => Kind::Inet,
            Net::Cidr(_) => Kind::Cidr,
            Net::Mac(_) => Kind::Mac,
            Net::Mac8(_) => Kind::Mac8,
        }
    }

    /// 0 for inet and cidr (they compare with each other), 1 for macaddr, 2 for macaddr8.
    fn class(&self) -> u8 {
        match self {
            Net::Inet(_) | Net::Cidr(_) => 0,
            Net::Mac(_) => 1,
            Net::Mac8(_) => 2,
        }
    }
}

impl fmt::Display for Net {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&print(self))
    }
}

// ---------------------------------------------------------------------------------------------
// Addresses as bits

impl Addr {
    fn full(v6: bool) -> u128 {
        if v6 { u128::MAX } else { u32::MAX as u128 }
    }

    fn maxbits(v6: bool) -> u32 {
        if v6 { 128 } else { 32 }
    }

    /// The top `bits` bits of an address of this family.
    fn mask(v6: bool, bits: u32) -> u128 {
        let full = Self::full(v6);
        full & !full.checked_shr(bits).unwrap_or(0)
    }

    /// The bits to the right of the mask.
    fn host(v6: bool, bits: u32) -> u128 {
        Self::full(v6) & !Self::mask(v6, bits)
    }

    fn network(&self) -> u128 {
        self.ip & Self::mask(self.v6, self.bits as u32)
    }

    /// The address in network order: 4 bytes or 16.
    fn octets(&self) -> Vec<u8> {
        let b = self.ip.to_be_bytes();
        if self.v6 { b.to_vec() } else { b[12..].to_vec() }
    }

    /// Postgres's `network_cmp`: family, then the network bits, then the mask length, then all bits.
    fn cmp_net(&self, o: &Addr) -> Ordering {
        self.v6
            .cmp(&o.v6)
            .then_with(|| {
                let m = Self::mask(self.v6, self.bits.min(o.bits) as u32);
                (self.ip & m).cmp(&(o.ip & m))
            })
            .then(self.bits.cmp(&o.bits))
            .then(self.ip.cmp(&o.ip))
    }

    /// Do the first `n` bits of the two addresses agree?
    fn same_prefix(&self, o: &Addr, n: u8) -> bool {
        (self.ip ^ o.ip) & Self::mask(self.v6, n as u32) == 0
    }
}

// ---------------------------------------------------------------------------------------------
// Errors

fn bad_input<T>(ty: &str, s: &str) -> Result<T> {
    err(BAD_INPUT, format!("invalid input syntax for type {ty}: \"{s}\""))
}

/// An error with a DETAIL line, which is all `Violation` is used for here.
fn with_detail<T>(code: &'static str, message: String, detail: &str) -> Result<T> {
    let info = Violation { detail: Some(detail.into()), ..Violation::default() };
    Err(SqlError { code, message, info: Some(Box::new(info)) })
}

fn out_of_range<T>() -> Result<T> {
    err(RANGE, "result is out of range")
}

/// 42883 for a function: no `name` takes these argument types (all of them are listed, as Postgres
/// does, with `int` shown as `integer`). Postgres adds a HINT that `SqlError` has no place for.
fn no_function<T>(name: &str, args: &[Ty]) -> Result<T> {
    let types: Vec<&str> = args.iter().map(|t| t.name()).collect();
    err(NO_FUNCTION, format!("function {name}({}) does not exist", types.join(", ")))
}

/// Does a binary operator `op` exist for operands of these two types, once each has been converted
/// the way Postgres would: `cidr` to `inet`, any integer to `bigint`? Not the mac-with-mac8 pairs: they
/// fit two operators at once, which is [`operator_error`]'s 42725.
fn operator_exists(op: &str, l: Ty, r: Ty) -> bool {
    use Kind::{Mac, Mac8};
    match op {
        // the same type on both sides, and one family: inet and cidr, or a mac's own width
        "=" | "<>" | "<" | "<=" | ">" | ">=" | "&" | "|" => {
            (l.is_ip() && r.is_ip()) || matches!((l, r), (Ty::Net(Mac), Ty::Net(Mac)) | (Ty::Net(Mac8), Ty::Net(Mac8)))
        }
        "<<" | "<<=" | ">>" | ">>=" | "&&" => l.is_ip() && r.is_ip(),
        // inet + bigint, bigint + inet; but only inet - bigint and inet - inet
        "+" => (l.is_ip() && r.is_int()) || (l.is_int() && r.is_ip()),
        "-" => l.is_ip() && (r.is_int() || r.is_ip()),
        _ => false,
    }
}

/// What Postgres says when no operator `l op r` fits: 42883, `operator does not exist: inet & macaddr`
/// (the types are the operands' own, cidr stays cidr). When both macaddr and macaddr8 can stand in for
/// the other (an implicit cast each way) two operators fit equally well, and it is 42725 instead.
fn operator_error<T>(op: &str, l: Ty, r: Ty) -> Result<T> {
    let mixed_macs =
        matches!((l, r), (Ty::Net(Kind::Mac), Ty::Net(Kind::Mac8)) | (Ty::Net(Kind::Mac8), Ty::Net(Kind::Mac)));
    if mixed_macs && matches!(op, "=" | "<>" | "<" | "<=" | ">" | ">=" | "&" | "|") {
        return err(AMBIGUOUS, format!("operator is not unique: {} {op} {}", l.name(), r.name()));
    }
    no_op(format!("operator does not exist: {} {op} {}", l.name(), r.name()))
}

/// Does the binary operator `op` (`= <> < <= > >= << <<= >> >>= && & | + -`) take operands of these
/// types? `Ok` if it does, else the error Postgres gives, 42883 or (for a macaddr with a macaddr8)
/// 42725. This is the check to run on a parsed `l op r` before evaluating it, whatever the operands
/// are: the operator functions below make the same check of their own arguments, but only a network
/// type can be given to them, and `inet & bigint` has no function to fail in. At least one operand
/// is a network type.
pub(super) fn resolve(op: &str, l: Ty, r: Ty) -> Result<()> {
    if operator_exists(op, l, r) { Ok(()) } else { operator_error(op, l, r) }
}

/// Does function `name` take arguments of these types? `Ok` if so, else 42883 listing all of them.
/// The same check as [`resolve`] makes for an operator, for a parsed call whose argument types are
/// exact: each function below checks the network arguments it is given, but one that takes an `i32`
/// can't tell a `smallint` from an `integer` when it reports `set_masklen(macaddr, smallint)`. At least
/// one argument is a network type (`text(5)` and `trunc(5)` are other functions' business). A call
/// written as a type's name, `macaddr(x)`, is the conversion to it.
pub(super) fn resolve_function(name: &str, args: &[Ty]) -> Result<()> {
    use Kind::{Mac, Mac8};
    let ok = match (name, args) {
        ("host" | "abbrev" | "broadcast" | "netmask" | "hostmask" | "network" | "masklen" | "family", [t]) => t.is_ip(),
        ("inet" | "cidr", [t]) => t.is_ip(),
        ("text", [Ty::Net(_)]) => true,
        ("trunc" | "macaddr8_set7bit" | "macaddr" | "macaddr8", [Ty::Net(Mac | Mac8)]) => true,
        ("set_masklen", [t, Ty::Int2 | Ty::Int4]) => t.is_ip(),
        ("inet_same_family" | "inet_merge", [a, b]) => a.is_ip() && b.is_ip(),
        _ => false,
    };
    if ok { Ok(()) } else { no_function(name, args) }
}

/// The address of an inet or cidr; a cidr converts to inet for free, as in Postgres.
fn addr(n: &Net) -> Option<&Addr> {
    match n {
        Net::Inet(a) | Net::Cidr(a) => Some(a),
        _ => None,
    }
}

/// The address of the one argument of function `name`.
fn ip<'a>(name: &str, n: &'a Net) -> Result<&'a Addr> {
    match addr(n) {
        Some(a) => Ok(a),
        None => no_function(name, &[n.ty()]),
    }
}

/// The addresses of the two arguments of function `name`; the message lists both types.
fn ip_pair<'a>(name: &str, a: &'a Net, b: &'a Net) -> Result<(&'a Addr, &'a Addr)> {
    match (addr(a), addr(b)) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => no_function(name, &[a.ty(), b.ty()]),
    }
}

/// The addresses of the two operands of an operator that only takes inet and cidr.
fn op_pair<'a>(op: &str, a: &'a Net, b: &'a Net) -> Result<(&'a Addr, &'a Addr)> {
    match (addr(a), addr(b)) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => operator_error(op, a.ty(), b.ty()),
    }
}

// ---------------------------------------------------------------------------------------------
// Input

/// Read `s` as a value of `kind`, as the type's input function does.
pub(super) fn parse(kind: Kind, s: &str) -> Result<Net> {
    // a C string ends at a NUL, and a SQL text has none; don't let one hide what follows it
    if s.contains('\0') {
        return bad_input(kind.name(), s);
    }
    match kind {
        Kind::Inet | Kind::Cidr => parse_ip(kind == Kind::Cidr, s),
        Kind::Mac => parse_mac(s),
        Kind::Mac8 => match parse_mac8(s.as_bytes()) {
            Some(m) => Ok(Net::Mac8(m)),
            None => bad_input("macaddr8", s),
        },
    }
}

fn parse_ip(cidr: bool, s: &str) -> Result<Net> {
    let ty = if cidr { "cidr" } else { "inet" };
    let b = s.as_bytes();
    // Postgres: a colon anywhere means IPv6
    let v6 = b.contains(&b':');
    let parsed = match (v6, cidr) {
        (true, _) => pton6(b),
        (false, true) => cidr_pton4(b),
        (false, false) => inet_pton4(b),
    };
    let Some((bits, ip)) = parsed else { return bad_input(ty, s) };
    let a = Addr { v6, bits: bits as u8, ip };
    if !cidr {
        return Ok(Net::Inet(a));
    }
    if a.ip & Addr::host(v6, bits) != 0 {
        return with_detail(BAD_INPUT, format!("invalid cidr value: \"{s}\""), "Value has bits set to right of mask.");
    }
    Ok(Net::Cidr(a))
}

/// A cursor over C-string bytes: reading past the end gives NUL, as a C parser would see.
struct Rd<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Rd<'a> {
    fn new(s: &'a [u8]) -> Rd<'a> {
        Rd { s, i: 0 }
    }

    fn at(&self, k: usize) -> u8 {
        self.s.get(self.i + k).copied().unwrap_or(0)
    }

    /// C's `*src++`.
    fn next(&mut self) -> u8 {
        let c = self.at(0);
        self.i += 1;
        c
    }

    fn rest(&self) -> &'a [u8] {
        self.s.get(self.i..).unwrap_or(&[])
    }
}

fn hexval(c: u8) -> Option<u32> {
    (c as char).to_digit(16)
}

/// Big-endian bytes as a number.
fn be(b: &[u8]) -> u128 {
    b.iter().fold(0u128, |a, &o| a << 8 | o as u128)
}

/// `/bits` after the octets, if a digit follows the slash; `ch` is the byte after the octets. The
/// digits add up in a 32-bit int that wraps (`/4294967296` is `/0`), and nothing may follow them.
/// -1 is no mask; `None` is a syntax error.
fn v4_bits(r: &mut Rd, mut ch: u8) -> Option<i32> {
    let mut bits = -1;
    if ch == b'/' && r.at(0).is_ascii_digit() {
        ch = r.next();
        bits = 0i32;
        loop {
            bits = bits.wrapping_mul(10).wrapping_add((ch - b'0') as i32);
            ch = r.next();
            if !ch.is_ascii_digit() {
                break;
            }
        }
    }
    if ch != 0 { None } else { Some(bits) }
}

/// Dotted decimal octets into `out` (each at most 255; leading zeros are fine); returns how many, and
/// the byte after them. With `dot_end` a `.` just before the end or a `/` is let go by.
fn v4_octets(r: &mut Rd, mut ch: u8, out: &mut [u8; 4], dot_end: bool) -> Option<(usize, u8)> {
    let mut n = 0;
    loop {
        let mut tmp = 0u32;
        loop {
            tmp = tmp * 10 + (ch - b'0') as u32;
            if tmp > 255 {
                return None;
            }
            ch = r.next();
            if !ch.is_ascii_digit() {
                break;
            }
        }
        if n == 4 {
            return None;
        }
        out[n] = tmp as u8;
        n += 1;
        if ch == 0 || ch == b'/' {
            return Some((n, ch));
        }
        if ch != b'.' {
            return None;
        }
        ch = r.next();
        if dot_end && (ch == 0 || ch == b'/') {
            return Some((n, ch));
        }
        if !ch.is_ascii_digit() {
            return None;
        }
    }
}

/// `inet`'s IPv4: `a.b.c.d` with an optional `/bits`; fewer octets need the `/bits`. Returns the
/// mask length and the address.
fn inet_pton4(s: &[u8]) -> Option<(u32, u128)> {
    let mut r = Rd::new(s);
    let ch = r.next();
    if !ch.is_ascii_digit() {
        return None;
    }
    let mut out = [0u8; 4];
    let (n, ch) = v4_octets(&mut r, ch, &mut out, true)?;
    let bits = match v4_bits(&mut r, ch)? {
        -1 if n == 4 => 32,
        -1 => return None,
        // the mask may reach into the next octet, but no further
        b if b >> 3 > n as i32 => return None,
        b => b,
    };
    (0..=32).contains(&bits).then_some((bits as u32, be(&out)))
}

/// `cidr`'s IPv4: the same, plus `0x` hex strings, and a missing `/bits` comes from the address's
/// class (`10.1` is `10.1.0.0/16`, `192.168` is `192.168.0.0/24`, `224` is `224.0.0.0/4`).
fn cidr_pton4(s: &[u8]) -> Option<(u32, u128)> {
    let mut r = Rd::new(s);
    let mut ch = r.next();
    let mut out = [0u8; 4];
    let n;
    if ch == b'0' && matches!(r.at(0), b'x' | b'X') && r.at(1).is_ascii_hexdigit() {
        r.i += 1;
        let (mut k, mut dirty, mut tmp) = (0, 0, 0u32);
        loop {
            ch = r.next();
            let Some(d) = hexval(ch) else { break };
            tmp = if dirty == 0 { d } else { tmp << 4 | d };
            dirty += 1;
            if dirty == 2 {
                *out.get_mut(k)? = tmp as u8;
                k += 1;
                dirty = 0;
            }
        }
        if dirty != 0 {
            *out.get_mut(k)? = (tmp << 4) as u8;
            k += 1;
        }
        n = k;
    } else if ch.is_ascii_digit() {
        let (k, c) = v4_octets(&mut r, ch, &mut out, false)?;
        n = k;
        ch = c;
    } else {
        return None;
    }
    let mut bits = v4_bits(&mut r, ch)?;
    if n == 0 {
        return None;
    }
    if bits == -1 {
        bits = match out[0] {
            240.. => 32,
            224.. => 8,
            192.. => 24,
            128.. => 16,
            _ => 8,
        };
        // an imputed mask narrower than the octets given widens to cover them
        bits = bits.max(n as i32 * 8);
        if bits == 8 && out[0] == 224 {
            bits = 4;
        }
    }
    (0..=32).contains(&bits).then_some((bits as u32, be(&out)))
}

/// The digits after the `/` of an IPv6 address: 0..=128, no leading zeros.
fn getbits(s: &[u8]) -> Option<i32> {
    let (mut val, mut n) = (0i32, 0);
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        if n != 0 && val == 0 {
            return None;
        }
        n += 1;
        val = val * 10 + (c - b'0') as i32;
        if val > 128 {
            return None;
        }
    }
    (n != 0).then_some(val)
}

/// The IPv4 tail of an IPv6 address (`::ffff:1.2.3.4/120`), from the start of its token. Quirky:
/// missing octets are zero (`1.2` is `1.2.0.0`), so are empty ones (`1..2`), but no octet has a
/// leading zero, and a trailing dot only works before a `/`. The mask after it is IPv6's, up to 128.
fn embedded4(t: &[u8]) -> Option<(i32, [u8; 4])> {
    let (dotted, bits) = match t.iter().position(|&c| c == b'/') {
        Some(i) => (&t[..i], Some(getbits(&t[i + 1..])?)),
        None => (t, None),
    };
    let fields: Vec<&[u8]> = dotted.split(|&c| c == b'.').collect();
    if fields.len() > 4 {
        return None;
    }
    let mut out = [0u8; 4];
    for (i, f) in fields.iter().enumerate() {
        if f.is_empty() {
            if i + 1 == fields.len() && bits.is_none() {
                return None;
            }
            continue;
        }
        if !f.iter().all(u8::is_ascii_digit) || (f[0] == b'0' && f.len() > 1) || f.len() > 3 {
            return None;
        }
        let v = f.iter().fold(0u32, |a, &c| a * 10 + (c - b'0') as u32);
        out[i] = u8::try_from(v).ok()?;
    }
    Some((bits.unwrap_or(-1), out))
}

/// IPv6 text into 16 bytes and a mask length (128 if none): hex words of up to four digits, one `::`,
/// an IPv4 tail.
fn pton6(s: &[u8]) -> Option<(u32, u128)> {
    let mut r = Rd::new(s);
    let mut tmp = [0u8; 16];
    let mut tp = 0;
    let mut colonp = None;
    // a leading :: needs special handling
    if r.at(0) == b':' {
        r.i += 1;
        if r.at(0) != b':' {
            return None;
        }
    }
    let mut curtok = r.i;
    let (mut saw_xdigit, mut val, mut digits, mut bits) = (false, 0u32, 0, -1);
    loop {
        let ch = r.next();
        if ch == 0 {
            break;
        }
        if let Some(d) = hexval(ch) {
            val = val << 4 | d;
            digits += 1;
            if digits > 4 {
                return None;
            }
            saw_xdigit = true;
            continue;
        }
        if ch == b':' {
            curtok = r.i;
            if !saw_xdigit {
                if colonp.is_some() {
                    return None;
                }
                colonp = Some(tp);
                continue;
            } else if r.at(0) == 0 {
                return None;
            }
            if tp + 2 > 16 {
                return None;
            }
            tmp[tp..tp + 2].copy_from_slice(&(val as u16).to_be_bytes());
            tp += 2;
            (saw_xdigit, digits, val) = (false, 0, 0);
            continue;
        }
        if ch == b'.'
            && tp + 4 <= 16
            && let Some((b, o)) = embedded4(&s[curtok..])
        {
            tmp[tp..tp + 4].copy_from_slice(&o);
            tp += 4;
            bits = b;
            saw_xdigit = false;
            break;
        }
        if ch == b'/'
            && let Some(b) = getbits(r.rest())
        {
            bits = b;
            break;
        }
        return None;
    }
    if saw_xdigit {
        if tp + 2 > 16 {
            return None;
        }
        tmp[tp..tp + 2].copy_from_slice(&(val as u16).to_be_bytes());
        tp += 2;
    }
    if bits == -1 {
        bits = 128;
    }
    if let Some(cp) = colonp {
        // the :: stands for however many zero words it takes; slide what follows it to the end
        if tp == 16 {
            return None;
        }
        let n = tp - cp;
        for i in 1..=n {
            tmp[16 - i] = tmp[cp + n - i];
            tmp[cp + n - i] = 0;
        }
        tp = 16;
    }
    (tp == 16).then_some((bits as u32, be(&tmp)))
}

// ---------------------------------------------------------------------------------------------
// macaddr: glibc's sscanf

/// One piece of a `sscanf` format: `%x`, `%2x`, a literal, or the `%1s` that catches trailing junk.
#[derive(Clone, Copy)]
enum Piece {
    X,
    X2,
    Lit(u8),
    Junk,
}

use Piece::{Junk, Lit, X, X2};

/// The formats `macaddr_in` tries, in its order (the dotted `%x.%x.%x.%x.%x.%x` is not among them).
const MAC_FORMATS: [&[Piece]; 7] = [
    &[X, Lit(b':'), X, Lit(b':'), X, Lit(b':'), X, Lit(b':'), X, Lit(b':'), X, Junk],
    &[X, Lit(b'-'), X, Lit(b'-'), X, Lit(b'-'), X, Lit(b'-'), X, Lit(b'-'), X, Junk],
    &[X2, X2, X2, Lit(b':'), X2, X2, X2, Junk],
    &[X2, X2, X2, Lit(b'-'), X2, X2, X2, Junk],
    &[X2, X2, Lit(b'.'), X2, X2, Lit(b'.'), X2, X2, Junk],
    &[X2, X2, Lit(b'-'), X2, X2, Lit(b'-'), X2, X2, Junk],
    &[X2, X2, X2, X2, X2, X2, Junk],
];

fn c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `%x` (or `%Nx` with `width`): skip spaces, an optional sign, an optional `0x`, then hex digits,
/// all within the width. The value is strtoul's: saturating, negated for `-`, cut to 32 bits.
fn scan_x(r: &mut Rd, width: Option<u32>) -> Option<u32> {
    while c_space(r.at(0)) {
        r.i += 1;
    }
    if r.at(0) == 0 {
        return None;
    }
    let mut w = width.unwrap_or(u32::MAX);
    let take = |r: &mut Rd, w: &mut u32| {
        r.i += 1;
        if width.is_some() {
            *w -= 1;
        }
    };
    let neg = r.at(0) == b'-';
    if matches!(r.at(0), b'-' | b'+') {
        take(r, &mut w);
    }
    let mut any = false;
    let (mut val, mut over) = (0u64, false);
    if w != 0 && r.at(0) == b'0' {
        any = true;
        take(r, &mut w);
        // the x of 0x is eaten even when no digit follows it
        if w != 0 && matches!(r.at(0), b'x' | b'X') {
            take(r, &mut w);
        }
    }
    while w != 0 && r.at(0).is_ascii_hexdigit() {
        any = true;
        let d = hexval(r.at(0))? as u64;
        match val.checked_mul(16).and_then(|v| v.checked_add(d)) {
            Some(v) => val = v,
            None => over = true,
        }
        take(r, &mut w);
    }
    if !any {
        return None;
    }
    let v = if over {
        u64::MAX
    } else if neg {
        val.wrapping_neg()
    } else {
        val
    };
    Some(v as u32)
}

/// Run one format: the six values if all six `%x` matched and nothing but spaces follows.
fn scan_mac(s: &[u8], fmt: &[Piece]) -> Option<[u32; 6]> {
    let mut r = Rd::new(s);
    let mut vals = [0u32; 6];
    let mut n = 0;
    for p in fmt {
        match *p {
            Piece::X | Piece::X2 => {
                vals[n] = scan_x(&mut r, matches!(p, Piece::X2).then_some(2))?;
                n += 1;
            }
            Piece::Lit(c) => {
                if r.at(0) != c {
                    return None;
                }
                r.i += 1;
            }
            Piece::Junk => {
                while c_space(r.at(0)) {
                    r.i += 1;
                }
                if r.at(0) != 0 {
                    return None;
                }
            }
        }
    }
    Some(vals)
}

fn parse_mac(s: &str) -> Result<Net> {
    let b = s.as_bytes();
    let Some(v) = MAC_FORMATS.iter().find_map(|f| scan_mac(b, f)) else { return bad_input("macaddr", s) };
    // a value that didn't fit a byte: sscanf stored it in an int
    if v.iter().any(|&x| x as i32 > 255 || (x as i32) < 0) {
        return err(RANGE, format!("invalid octet value in \"macaddr\" value: \"{s}\""));
    }
    Ok(Net::Mac(v.iter().fold(0u64, |a, &x| a << 8 | x as u64)))
}

/// `macaddr8`: pairs of hex digits, six or eight of them, joined by any one of `:`, `-`, `.` (the
/// same one all through) wherever the writer likes; six get `ff:fe` put in the middle. Spaces may
/// come before and after. Like Postgres, a last character with no partner is never looked at, so
/// `08:00:2b:01:02:03x` is fine.
fn parse_mac8(s: &[u8]) -> Option<u64> {
    let mut r = Rd::new(s);
    while c_space(r.at(0)) {
        r.i += 1;
    }
    let mut bytes = [0u8; 8];
    let mut count = 0;
    let mut spacer = 0u8;
    // digits always come in pairs
    while r.at(0) != 0 && r.at(1) != 0 {
        if c_space(r.at(0)) {
            // trailing spaces, and then nothing
            while c_space(r.at(0)) {
                r.i += 1;
            }
            if r.at(0) != 0 {
                return None;
            }
            break;
        }
        let (h, l) = (hexval(r.at(0))?, hexval(r.at(1))?);
        *bytes.get_mut(count)? = (h << 4 | l) as u8;
        count += 1;
        r.i += 2;
        if matches!(r.at(0), b':' | b'-' | b'.') {
            if spacer == 0 {
                spacer = r.at(0);
            } else if spacer != r.at(0) {
                return None;
            }
            r.i += 1;
        }
    }
    match count {
        6 => bytes = [bytes[0], bytes[1], bytes[2], 0xff, 0xfe, bytes[3], bytes[4], bytes[5]],
        8 => {}
        _ => return None,
    }
    Some(u64::from_be_bytes(bytes))
}

// ---------------------------------------------------------------------------------------------
// Output

/// The type's output function: what a SELECT of the value shows.
pub(super) fn print(n: &Net) -> String {
    match n {
        Net::Inet(a) => ntop(a, false),
        Net::Cidr(a) => ntop(a, true),
        Net::Mac(m) => hex_bytes(&m.to_be_bytes()[2..]),
        Net::Mac8(m) => hex_bytes(&m.to_be_bytes()),
    }
}

fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":")
}

fn dotted(b: &[u8]) -> String {
    b.iter().map(u8::to_string).collect::<Vec<_>>().join(".")
}

/// A run of zero words: where it starts, and how many.
type Run = (usize, usize);

/// The longest run of zero words (the first, on a tie), if it is at least two long.
fn zero_run(w: &[u32]) -> Option<Run> {
    let mut best: Option<Run> = None;
    let mut i = 0;
    while i < w.len() {
        let start = i;
        while i < w.len() && w[i] == 0 {
            i += 1;
        }
        if i > start && best.is_none_or(|b| i - start > b.1) {
            best = Some((start, i - start));
        }
        i += 1;
    }
    best.filter(|b| b.1 >= 2)
}

/// The address alone, all four octets or the IPv6 shortest form: `::ffff:1.2.3.4`.
fn ntop_addr(a: &Addr) -> String {
    if !a.v6 {
        return dotted(&a.octets());
    }
    let w: Vec<u32> = (0..8).map(|i| (a.ip >> (112 - 16 * i)) as u32 & 0xffff).collect();
    let best = zero_run(&w);
    // an address that is really an IPv4 one: ::1.2.3.4 and ::ffff:1.2.3.4
    let v4_tail = match best {
        Some((0, 6)) => true,
        Some((0, 5)) => w[5] == 0xffff,
        _ => false,
    };
    let mut o = String::new();
    for (i, &word) in w.iter().enumerate() {
        if let Some((base, len)) = best
            && (base..base + len).contains(&i)
        {
            if i == base {
                o.push(':');
            }
            continue;
        }
        if i != 0 {
            o.push(':');
        }
        if i == 6 && v4_tail {
            o.push_str(&dotted(&a.octets()[12..]));
            return o;
        }
        o.push_str(&format!("{word:x}"));
    }
    if best.is_some_and(|(b, l)| b + l == 8) {
        o.push(':');
    }
    o
}

/// `inet_out` and `cidr_out`: the `/bits` is left off a host address, unless it is a cidr.
fn ntop(a: &Addr, always: bool) -> String {
    let mut s = ntop_addr(a);
    if always || a.bits as u32 != Addr::maxbits(a.v6) {
        s.push_str(&format!("/{}", a.bits));
    }
    s
}

/// `abbrev(cidr)`: the network's short form, `10.1/16`; for IPv6 only as many words as the mask
/// reaches, and so `2001:db8:0:1::/64` comes out `2001:db8::1/64` (Postgres's own behaviour).
fn cidr_abbrev(a: &Addr) -> String {
    let bits = a.bits as u32;
    let net = a.network();
    if !a.v6 {
        let b = a.octets();
        let mut o = String::new();
        if bits == 0 {
            o.push('0');
        }
        let whole = (bits / 8) as usize;
        o.push_str(&dotted(&b[..whole]));
        if !bits.is_multiple_of(8) {
            if whole > 0 {
                o.push('.');
            }
            o.push_str(&(b[whole] & (0xffu32 << (8 - bits % 8)) as u8).to_string());
        }
        return format!("{o}/{bits}");
    }
    if bits == 0 {
        return "::/0".into();
    }
    let s = net.to_be_bytes();
    let words = bits.div_ceil(16).max(2) as usize;
    let w: Vec<u32> = (0..words).map(|i| (s[2 * i] as u32) << 8 | s[2 * i + 1] as u32).collect();
    // Postgres's search for the longest run of zero words. A run that doesn't beat the best leaves
    // the running count as it was, so zeros after it add to it: [0 0 1 0 0 2 0] finds a run of 3
    // that takes in the 2. Copied as it is, since the output has to match.
    let (mut zs, mut zl, mut ts, mut tl) = (0, 0, 0, 0);
    for (i, &x) in w.iter().enumerate() {
        if x == 0 {
            if tl == 0 {
                ts = i;
            }
            tl += 1;
        } else if tl != 0 && zl < tl {
            (zs, zl, tl) = (ts, tl, 0);
        }
    }
    if tl != 0 && zl < tl {
        (zs, zl) = (ts, tl);
    }
    let nb = bits.div_ceil(8) as usize;
    let mut o = String::new();
    // an address that looks IPv4 in it: as many of the octets as the mask reaches (two at least, for
    // ::a.b and ::ffff:a.b). A lone word is one only when its high byte is set and its low one isn't 1,
    // and then it comes out as `:.1.2`: Postgres's own doing
    let tail = zl != words && zs == 0 && bits > 96;
    if tail && (zl == 6 || (zl == 5 && w[5] == 0xffff)) {
        o.push_str(if zl == 6 { "::" } else { "::ffff:" });
        o.push_str(&dotted(&s[12..12 + nb.saturating_sub(12).max(2)]));
    } else if tail && zl == 7 && s[14] != 0 && s[15] != 1 {
        o.push(':');
        for x in &s[14..nb] {
            o.push_str(&format!(".{x}"));
        }
    } else {
        let hex = |r: &[u32]| r.iter().map(|x| format!("{x:x}")).collect::<Vec<_>>().join(":");
        if zl == 0 {
            o.push_str(&hex(&w));
        } else {
            o.push_str(&hex(&w[..zs]));
            o.push_str("::");
            o.push_str(&hex(w.get(zs + zl..).unwrap_or(&[])));
        }
    }
    format!("{o}/{bits}")
}

// ---------------------------------------------------------------------------------------------
// Ordering and storage

/// The comparison behind SQL's `=`, `<>`, `<`, `<=`, `>` and `>=`: inet and cidr by Postgres's
/// `network_cmp` (an inet and a cidr of the same network are equal, as they are in SQL, where the
/// cidr is cast to inet first), macs by value. Values of unrelated types order by type, so that `Ord`
/// can be total; that is not an answer SQL ever gives, and the operators go through [`compare_with`],
/// which refuses those pairs. `Ord` is this, with the kind as a last tie-break so that it agrees with
/// `==`.
pub(super) fn compare(a: &Net, b: &Net) -> Ordering {
    match (a, b) {
        (Net::Inet(x) | Net::Cidr(x), Net::Inet(y) | Net::Cidr(y)) => x.cmp_net(y),
        (Net::Mac(x), Net::Mac(y)) | (Net::Mac8(x), Net::Mac8(y)) => x.cmp(y),
        _ => a.class().cmp(&b.class()),
    }
}

/// One of SQL's six comparison operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    fn sym(self) -> &'static str {
        match self {
            Cmp::Eq => "=",
            Cmp::Ne => "<>",
            Cmp::Lt => "<",
            Cmp::Le => "<=",
            Cmp::Gt => ">",
            Cmp::Ge => ">=",
        }
    }

    fn holds(self, o: Ordering) -> bool {
        match self {
            Cmp::Eq => o.is_eq(),
            Cmp::Ne => o.is_ne(),
            Cmp::Lt => o.is_lt(),
            Cmp::Le => o.is_le(),
            Cmp::Gt => o.is_gt(),
            Cmp::Ge => o.is_ge(),
        }
    }
}

/// `a op b` as SQL evaluates it: `compare`, once the operand types have found an operator. inet and
/// cidr compare with each other; a macaddr only with a macaddr and a macaddr8 with a macaddr8. Any
/// other pair is 42883, and a macaddr with a macaddr8 is 42725 (each converts to the other, so two
/// operators fit).
pub(super) fn compare_with(op: Cmp, a: &Net, b: &Net) -> Result<bool> {
    resolve(op.sym(), a.ty(), b.ty())?;
    Ok(op.holds(compare(a, b)))
}

/// A total order, for sorting and B-tree keys: `compare`, and then inet before cidr.
impl Ord for Net {
    fn cmp(&self, o: &Net) -> Ordering {
        compare(self, o).then_with(|| (self.kind() as u8).cmp(&(o.kind() as u8)))
    }
}

impl PartialOrd for Net {
    fn partial_cmp(&self, o: &Net) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

// Stored form: a tag byte, then
//   IPv4 (0x04) / IPv6 (0x06): the network bits (the address with its host bits zeroed, 4 or 16
//     bytes), the mask length, the whole address, then 0 for inet or 1 for cidr;
//   macaddr (0x10): 6 bytes; macaddr8 (0x11): 8 bytes.
// Comparing two such byte strings with memcmp orders the values as `Ord` does: the network bits
// padded with zeros sort as the bit strings do (a shorter mask first), equal networks fall to the
// mask length and then the whole address, exactly `network_cmp`.
const TAG_V4: u8 = 0x04;
const TAG_V6: u8 = 0x06;
const TAG_MAC: u8 = 0x10;
const TAG_MAC8: u8 = 0x11;

pub(super) fn encode(n: &Net) -> Vec<u8> {
    match n {
        Net::Inet(a) | Net::Cidr(a) => {
            let mut o = vec![if a.v6 { TAG_V6 } else { TAG_V4 }];
            o.extend(Addr { ip: a.network(), ..*a }.octets());
            o.push(a.bits);
            o.extend(a.octets());
            o.push(matches!(n, Net::Cidr(_)) as u8);
            o
        }
        Net::Mac(m) => [&[TAG_MAC][..], &m.to_be_bytes()[2..]].concat(),
        Net::Mac8(m) => [&[TAG_MAC8][..], &m.to_be_bytes()[..]].concat(),
    }
}

/// The inverse of `encode`; `None` for bytes it could not have made.
pub(super) fn decode(b: &[u8]) -> Option<Net> {
    match *b.first()? {
        t @ (TAG_V4 | TAG_V6) => {
            let v6 = t == TAG_V6;
            let w = if v6 { 16 } else { 4 };
            if b.len() != 2 * w + 3 {
                return None;
            }
            let (net, bits, ip, cidr) = (be(&b[1..1 + w]), b[1 + w], be(&b[2 + w..2 + 2 * w]), b[2 + 2 * w]);
            let a = Addr { v6, bits, ip };
            let ok = bits as u32 <= Addr::maxbits(v6) && a.network() == net && cidr <= 1;
            if !ok || (cidr == 1 && ip != net) {
                return None;
            }
            Some(if cidr == 1 { Net::Cidr(a) } else { Net::Inet(a) })
        }
        TAG_MAC if b.len() == 7 => Some(Net::Mac(be(&b[1..]) as u64)),
        TAG_MAC8 if b.len() == 9 => Some(Net::Mac8(be(&b[1..]) as u64)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// In SQL

/// The SQL type a value has.
pub(super) fn ty_of(n: &Net) -> Type {
    match n {
        Net::Inet(_) => Type::Inet,
        Net::Cidr(_) => Type::Cidr,
        Net::Mac(_) => Type::MacAddr,
        Net::Mac8(_) => Type::MacAddr8,
    }
}

/// The network type a column type is, if it is one.
pub(super) fn kind_of(t: Type) -> Option<Kind> {
    match t {
        Type::Inet => Some(Kind::Inet),
        Type::Cidr => Some(Kind::Cidr),
        Type::MacAddr => Some(Kind::Mac),
        Type::MacAddr8 => Some(Kind::Mac8),
        _ => None,
    }
}

/// The operand type [`resolve`] and [`resolve_function`] want for a SQL type: a network type, or the
/// integer they take (Postgres would name a smallint column's type `smallint`, which `Type::Int`
/// doesn't say).
pub(super) fn ty_for(t: Type) -> Option<Ty> {
    match t {
        Type::SmallInt | Type::Int => Some(Ty::Int4),
        Type::BigInt => Some(Ty::Int8),
        t => kind_of(t).map(Ty::Net),
    }
}

/// The type an operator reads an untyped literal beside a value of this one as: inet for a cidr, whose
/// operators are all inet's (`cidr_col << '10.0.0.1/16'` is fine).
pub(super) fn operand_kind(n: &Net) -> Kind {
    match n.kind() {
        Kind::Cidr => Kind::Inet,
        k => k,
    }
}

/// Whether the two values have a comparison operator between them (an inet with a cidr, a mac with a
/// mac of its own width); `compare_with` gives the error when they have none.
pub(super) fn comparable(a: &Net, b: &Net) -> bool {
    a.class() == b.class()
}

/// What hashing and grouping see: equal values (an inet and a cidr of the same address and mask are
/// equal) give equal keys, and no two others do.
pub(super) fn hash_key(n: &Net) -> (u8, u8, u128) {
    match n {
        Net::Inet(a) | Net::Cidr(a) => (a.v6 as u8, a.bits, a.ip),
        Net::Mac(m) => (2, 0, *m as u128),
        Net::Mac8(m) => (3, 0, *m as u128),
    }
}

// ---------------------------------------------------------------------------------------------
// The wire's binary form: what `inet_send`, `cidr_send`, `macaddr_send` and `macaddr8_send` write
// and their `_recv` read. An inet or cidr is its family (2 for IPv4, 3 for IPv6), its mask length,
// 1 if it is a cidr, the address's length and the address; a mac is its bytes.

pub(super) fn to_wire(n: &Net) -> Vec<u8> {
    match n {
        Net::Inet(a) | Net::Cidr(a) => {
            let ip = a.octets();
            let mut o = vec![if a.v6 { 3 } else { 2 }, a.bits, matches!(n, Net::Cidr(_)) as u8, ip.len() as u8];
            o.extend(ip);
            o
        }
        Net::Mac(m) => m.to_be_bytes()[2..].to_vec(),
        Net::Mac8(m) => m.to_be_bytes().to_vec(),
    }
}

/// A value of `kind` from its binary form, refused as `network_recv`, `macaddr_recv` and
/// `macaddr8_recv` refuse it (22P03): the family, the mask length and the address length are checked,
/// and a cidr may have no bits set past its mask.
pub(super) fn from_wire(kind: Kind, b: &[u8]) -> Result<Net> {
    let bad = |what: &str| err(INVALID_BINARY, format!("invalid {what} in external \"{}\" value", kind.name()));
    match kind {
        Kind::Inet | Kind::Cidr => {
            let [family, bits, _is_cidr, len, ip @ ..] = b else {
                return err(PROTOCOL, "insufficient data left in message");
            };
            let v6 = match family {
                2 => false,
                3 => true,
                _ => return bad("address family"),
            };
            if *bits as u32 > Addr::maxbits(v6) {
                return bad("bits");
            }
            if *len as usize != if v6 { 16 } else { 4 } {
                return bad("length");
            }
            if ip.len() < *len as usize {
                return err(PROTOCOL, "insufficient data left in message");
            }
            if ip.len() > *len as usize {
                return err(INVALID_BINARY, "incorrect binary data format");
            }
            let a = Addr { v6, bits: *bits, ip: be(ip) };
            if kind == Kind::Cidr {
                if a.network() != a.ip {
                    return err(INVALID_BINARY, "invalid external \"cidr\" value");
                }
                return Ok(Net::Cidr(a));
            }
            Ok(Net::Inet(a))
        }
        Kind::Mac | Kind::Mac8 => {
            let n = if kind == Kind::Mac { 6 } else { 8 };
            if b.len() < n {
                return err(PROTOCOL, "insufficient data left in message");
            }
            if b.len() > n {
                return err(INVALID_BINARY, "incorrect binary data format");
            }
            let m = be(b) as u64;
            Ok(if kind == Kind::Mac { Net::Mac(m) } else { Net::Mac8(m) })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Operators on inet and cidr

/// `a << b`: a is strictly inside b.
pub(super) fn within(a: &Net, b: &Net) -> Result<bool> {
    let (x, y) = op_pair("<<", a, b)?;
    Ok(x.v6 == y.v6 && x.bits > y.bits && x.same_prefix(y, y.bits))
}

/// `a <<= b`: inside or equal.
pub(super) fn within_eq(a: &Net, b: &Net) -> Result<bool> {
    let (x, y) = op_pair("<<=", a, b)?;
    Ok(x.v6 == y.v6 && x.bits >= y.bits && x.same_prefix(y, y.bits))
}

/// `a >> b`: a strictly contains b.
pub(super) fn contains(a: &Net, b: &Net) -> Result<bool> {
    let (x, y) = op_pair(">>", a, b)?;
    Ok(x.v6 == y.v6 && x.bits < y.bits && x.same_prefix(y, x.bits))
}

/// `a >>= b`: contains or equal.
pub(super) fn contains_eq(a: &Net, b: &Net) -> Result<bool> {
    let (x, y) = op_pair(">>=", a, b)?;
    Ok(x.v6 == y.v6 && x.bits <= y.bits && x.same_prefix(y, x.bits))
}

/// `a && b`: one contains the other.
pub(super) fn overlaps(a: &Net, b: &Net) -> Result<bool> {
    let (x, y) = op_pair("&&", a, b)?;
    Ok(x.v6 == y.v6 && x.same_prefix(y, x.bits.min(y.bits)))
}

/// `~ a`: every address bit flipped; the mask length stays. Also macaddr and macaddr8.
pub(super) fn not(a: &Net) -> Result<Net> {
    match a {
        Net::Inet(x) | Net::Cidr(x) => Ok(Net::Inet(Addr { ip: !x.ip & Addr::full(x.v6), ..*x })),
        Net::Mac(m) => Ok(Net::Mac(!m & 0xffff_ffff_ffff)),
        Net::Mac8(m) => Ok(Net::Mac8(!m)),
    }
}

/// `a & b`, `a | b`: per bit; the result's mask length is the larger of the two.
fn bitwise(name: &str, a: &Net, b: &Net, f: fn(u64, u64) -> u64, g: fn(u128, u128) -> u128) -> Result<Net> {
    match (a, b) {
        (Net::Inet(x) | Net::Cidr(x), Net::Inet(y) | Net::Cidr(y)) => {
            if x.v6 != y.v6 {
                let word = if name == "&" { "AND" } else { "OR" };
                return err(BAD_ARG, format!("cannot {word} inet values of different sizes"));
            }
            Ok(Net::Inet(Addr { v6: x.v6, bits: x.bits.max(y.bits), ip: g(x.ip, y.ip) }))
        }
        (Net::Mac(x), Net::Mac(y)) => Ok(Net::Mac(f(*x, *y))),
        (Net::Mac8(x), Net::Mac8(y)) => Ok(Net::Mac8(f(*x, *y))),
        _ => operator_error(name, a.ty(), b.ty()),
    }
}

pub(super) fn and(a: &Net, b: &Net) -> Result<Net> {
    bitwise("&", a, b, |x, y| x & y, |x, y| x & y)
}

pub(super) fn or(a: &Net, b: &Net) -> Result<Net> {
    bitwise("|", a, b, |x, y| x | y, |x, y| x | y)
}

/// `a + n`: an inet (a cidr first becomes one); out of range (22003) past either end of the address
/// space. A macaddr or an inet for `a` is `operator does not exist: macaddr + bigint`.
pub(super) fn add(a: &Net, n: i64) -> Result<Net> {
    offset(a, n, "+", false)
}

/// `n + a`: the same sum, with the operands the other way round (which only a failure shows).
pub(super) fn add_to(n: i64, a: &Net) -> Result<Net> {
    offset(a, n, "+", true)
}

/// `a - n`. Postgres negates `n` in 64 bits first, so `a - (-2^63)` is `a + (-2^63)`. There is no
/// `n - a`.
pub(super) fn sub(a: &Net, n: i64) -> Result<Net> {
    offset(a, n.wrapping_neg(), "-", false)
}

/// The sum or difference behind `add`, `add_to` and `sub`. `op` and `int_first` say which operator to
/// name if `a` is no inet; the integer is a bigint, since that is what the operator takes.
fn offset(a: &Net, n: i64, op: &str, int_first: bool) -> Result<Net> {
    let Some(x) = addr(a) else {
        return if int_first { operator_error(op, Ty::Int8, a.ty()) } else { operator_error(op, a.ty(), Ty::Int8) };
    };
    let v = match n >= 0 {
        true => x.ip.checked_add(n as u128),
        false => x.ip.checked_sub(n.unsigned_abs() as u128),
    };
    match v.filter(|&v| v <= Addr::full(x.v6)) {
        Some(ip) => Ok(Net::Inet(Addr { ip, ..*x })),
        None => out_of_range(),
    }
}

/// `a - b` for two inets: the distance, as a bigint. Postgres adds in two's complement byte by byte,
/// so in IPv6 a difference of 2^128 + 1 is 1; one too big for 64 bits is out of range (22003).
pub(super) fn diff(a: &Net, b: &Net) -> Result<i64> {
    let (x, y) = op_pair("-", a, b)?;
    if x.v6 != y.v6 {
        return err(BAD_ARG, "cannot subtract inet values of different sizes");
    }
    let (pa, pb) = (x.octets(), y.octets());
    let mut res = 0u64;
    let mut carry = 1i32;
    for (byte, nb) in (0..pa.len()).rev().enumerate() {
        carry += pa[nb] as i32 + (!pb[nb]) as i32;
        let lo = (carry & 0xff) as u64;
        if byte < 8 {
            res |= lo << (byte * 8);
        } else if (res as i64) < 0 && lo != 0xff || (res as i64) >= 0 && lo != 0 {
            // what doesn't fit must be all ones or all zeros, as the sign says
            return out_of_range();
        }
        carry >>= 8;
    }
    // an input narrower than 64 bits can't overflow, but needs its sign extended
    if carry == 0 && pa.len() < 8 {
        res |= u64::MAX << (pa.len() * 8);
    }
    Ok(res as i64)
}

// ---------------------------------------------------------------------------------------------
// Functions on inet and cidr

/// `host(a)`: the address without its mask.
pub(super) fn host(a: &Net) -> Result<String> {
    Ok(ntop_addr(ip("host", a)?))
}

/// `text(a)`: the address and its mask length, always (`1.2.3.4/32`). For a macaddr or a macaddr8 it
/// is the plain output text: `text(x)` on a type with no such function is a cast to text, which
/// goes through the type's output function.
pub(super) fn text(a: &Net) -> Result<String> {
    match addr(a) {
        Some(x) => Ok(format!("{}/{}", ntop_addr(x), x.bits)),
        None => Ok(print(a)),
    }
}

/// `abbrev(a)`: what `print` shows for an inet; for a cidr the shortest form (`10.1/16`).
pub(super) fn abbrev(a: &Net) -> Result<String> {
    match a {
        Net::Cidr(x) => Ok(cidr_abbrev(x)),
        _ => Ok(print(&Net::Inet(*ip("abbrev", a)?))),
    }
}

/// `broadcast(a)`: the address with every bit to the right of the mask set; an inet.
pub(super) fn broadcast(a: &Net) -> Result<Net> {
    let x = ip("broadcast", a)?;
    Ok(Net::Inet(Addr { ip: x.ip | Addr::host(x.v6, x.bits as u32), ..*x }))
}

/// `network(a)`: the address with the bits to the right of the mask cleared; a cidr.
pub(super) fn network(a: &Net) -> Result<Net> {
    let x = ip("network", a)?;
    Ok(Net::Cidr(Addr { ip: x.network(), ..*x }))
}

/// `netmask(a)`: ones for the mask's bits, zeros after, as a host address.
pub(super) fn netmask(a: &Net) -> Result<Net> {
    let x = ip("netmask", a)?;
    let bits = Addr::maxbits(x.v6) as u8;
    Ok(Net::Inet(Addr { v6: x.v6, bits, ip: Addr::mask(x.v6, x.bits as u32) }))
}

/// `hostmask(a)`: the opposite, zeros then ones.
pub(super) fn hostmask(a: &Net) -> Result<Net> {
    let x = ip("hostmask", a)?;
    let bits = Addr::maxbits(x.v6) as u8;
    Ok(Net::Inet(Addr { v6: x.v6, bits, ip: Addr::host(x.v6, x.bits as u32) }))
}

pub(super) fn masklen(a: &Net) -> Result<i32> {
    Ok(ip("masklen", a)?.bits as i32)
}

/// 4 or 6.
pub(super) fn family(a: &Net) -> Result<i32> {
    Ok(if ip("family", a)?.v6 { 6 } else { 4 })
}

pub(super) fn inet_same_family(a: &Net, b: &Net) -> Result<bool> {
    let (x, y) = ip_pair("inet_same_family", a, b)?;
    Ok(x.v6 == y.v6)
}

/// `set_masklen(a, n)`: -1 means the whole address. For a cidr the bits newly to the right of the
/// mask are cleared; for an inet they stay.
pub(super) fn set_masklen(a: &Net, bits: i32) -> Result<Net> {
    let Some(x) = addr(a) else { return no_function("set_masklen", &[a.ty(), Ty::Int4]) };
    let max = Addr::maxbits(x.v6) as i32;
    let bits = if bits == -1 { max } else { bits };
    if !(0..=max).contains(&bits) {
        return err(BAD_ARG, format!("invalid mask length: {bits}"));
    }
    let y = Addr { bits: bits as u8, ..*x };
    Ok(match a {
        Net::Cidr(_) => Net::Cidr(Addr { ip: y.network(), ..y }),
        _ => Net::Inet(y),
    })
}

/// `inet_merge(a, b)`: the smallest network holding both; a cidr.
pub(super) fn inet_merge(a: &Net, b: &Net) -> Result<Net> {
    let (x, y) = ip_pair("inet_merge", a, b)?;
    if x.v6 != y.v6 {
        return err(BAD_ARG, "cannot merge addresses from different families");
    }
    let width = Addr::maxbits(x.v6);
    let xor = x.ip ^ y.ip;
    let same = if xor == 0 { width } else { xor.leading_zeros() - (128 - width) };
    let bits = same.min(x.bits.min(y.bits) as u32);
    let ip = x.ip & Addr::mask(x.v6, bits);
    Ok(Net::Cidr(Addr { v6: x.v6, bits: bits as u8, ip }))
}

/// How a conversion was written, which only matters to what Postgres says when it can't be done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Syntax {
    /// `x::macaddr` or `CAST(x AS macaddr)`: no cast between the types is 42846, `cannot cast type
    /// inet to macaddr`.
    Cast,
    /// `macaddr(x)`: a call of a function with the type's name, which is 42883, `function
    /// macaddr(inet) does not exist`, if nothing by that name takes the argument.
    Call,
}

/// `a` as a value of type `to`, by whichever of Postgres's conversions there is:
///   * the same type is itself, unchanged;
///   * `cidr` to `inet` is free, and `inet` to `cidr` clears the host bits (never an error);
///   * `macaddr` to `macaddr8` puts `ff:fe` in the middle, and back only works for an address that
///     has it (22003);
///   * every other pair, an `inet` or `cidr` with a mac, is no conversion at all.
pub(super) fn convert(a: &Net, to: Kind, how: Syntax) -> Result<Net> {
    match (a, to) {
        _ if a.kind() == to => Ok(a.clone()),
        (Net::Cidr(x), Kind::Inet) => Ok(Net::Inet(*x)),
        (Net::Inet(x), Kind::Cidr) => Ok(Net::Cidr(Addr { ip: x.network(), ..*x })),
        (Net::Mac(x), Kind::Mac8) => Ok(Net::Mac8((x >> 24) << 40 | 0xfffe << 24 | x & 0xff_ffff)),
        (Net::Mac8(x), Kind::Mac) if (x >> 24) & 0xffff == 0xfffe => Ok(Net::Mac((x >> 40) << 24 | x & 0xff_ffff)),
        (Net::Mac8(_), Kind::Mac) => err(RANGE, "macaddr8 data out of range to convert to macaddr"),
        _ => match how {
            Syntax::Cast => err(CANNOT_COERCE, format!("cannot cast type {} to {}", a.kind().name(), to.name())),
            Syntax::Call => no_function(to.name(), &[a.ty()]),
        },
    }
}

/// `a::cidr`: the host bits are cleared (never an error). See [`convert`] for `cidr(a)`.
#[cfg(test)]
pub(super) fn to_cidr(a: &Net) -> Result<Net> {
    convert(a, Kind::Cidr, Syntax::Cast)
}

/// `a::inet` for a cidr (and for an inet, unchanged).
#[cfg(test)]
pub(super) fn to_inet(a: &Net) -> Result<Net> {
    convert(a, Kind::Inet, Syntax::Cast)
}

// ---------------------------------------------------------------------------------------------
// macaddr and macaddr8

/// `trunc(m)`: the last three bytes of a macaddr, the last five of a macaddr8, zeroed.
pub(super) fn trunc(m: &Net) -> Result<Net> {
    match m {
        Net::Mac(x) => Ok(Net::Mac(x & 0xffff_ff00_0000)),
        Net::Mac8(x) => Ok(Net::Mac8(x & 0xffff_ff00_0000_0000)),
        _ => no_function("trunc", &[m.ty()]),
    }
}

/// `m::macaddr8`: `ff:fe` goes in the middle. A macaddr8 is itself.
pub(super) fn mac_to_mac8(m: &Net) -> Result<Net> {
    convert(m, Kind::Mac8, Syntax::Cast)
}

/// `m::macaddr`: only an address with `ff:fe` in the middle comes back (22003). A macaddr is itself.
#[cfg(test)]
pub(super) fn mac8_to_mac(m: &Net) -> Result<Net> {
    convert(m, Kind::Mac, Syntax::Cast)
}

/// `macaddr8_set7bit(m)`: the 7th bit of the first byte set (the modified EUI-64 of an IPv6 address).
/// A macaddr is converted to a macaddr8 first (the cast is implicit), so it gets its `ff:fe` too.
pub(super) fn macaddr8_set7bit(m: &Net) -> Result<Net> {
    match m {
        Net::Mac8(x) => Ok(Net::Mac8(x | 0x0200_0000_0000_0000)),
        Net::Mac(_) => macaddr8_set7bit(&mac_to_mac8(m)?),
        _ => no_function("macaddr8_set7bit", &[m.ty()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An argument written as `type:text`: `inet:1.2.3.4/24`, `cidr:10.0.0.0/8`, `macaddr:...`,
    /// `macaddr8:...`, or an integer as `smallint:5`, `int:5` or `bigint:5` (Postgres names the type
    /// in an error, so it has to be said).
    enum Arg {
        Net(Net),
        Int(i64, Ty),
    }

    impl Arg {
        fn ty(&self) -> Ty {
            match self {
                Arg::Net(n) => n.ty(),
                Arg::Int(_, t) => *t,
            }
        }
    }

    fn arg(s: &str) -> Result<Arg> {
        let (ty, v) = s.split_once(':').expect("type:text");
        let int = |t| Arg::Int(v.parse().expect("int"), t);
        Ok(match ty {
            "inet" => Arg::Net(parse(Kind::Inet, v)?),
            "cidr" => Arg::Net(parse(Kind::Cidr, v)?),
            "macaddr" => Arg::Net(parse(Kind::Mac, v)?),
            "macaddr8" => Arg::Net(parse(Kind::Mac8, v)?),
            "smallint" => int(Ty::Int2),
            "int" => int(Ty::Int4),
            "bigint" => int(Ty::Int8),
            _ => panic!("type {ty}"),
        })
    }

    enum Out {
        Net(Net),
        Bool(bool),
        Int(i64),
        Text(String),
    }

    const OPERATORS: [&str; 15] = ["=", "<>", "<", "<=", ">", ">=", "<<", "<<=", ">>", ">>=", "&&", "&", "|", "+", "-"];

    /// One operator or function over typed arguments, named as in SQL; the four type names alone
    /// (`inet`, `cidr`, `macaddr`, `macaddr8`) read their one argument as plain input text. `to_inet`,
    /// `to_cidr`, `to_macaddr` and `to_macaddr8` are the casts `x::inet` and so on, and `fn_inet`,
    /// `fn_cidr`, `fn_macaddr` and `fn_macaddr8` the calls `inet(x)`; an argument list no overload
    /// takes is what Postgres says of it: an operator's operand types go to `resolve`, anything else
    /// is a function that does not exist.
    fn call(op: &str, args: &[&str]) -> Result<Out> {
        for (name, kind) in
            [("inet", Kind::Inet), ("cidr", Kind::Cidr), ("macaddr", Kind::Mac), ("macaddr8", Kind::Mac8)]
        {
            if op == name && args.len() == 1 {
                return parse(kind, args[0]).map(Out::Net);
            }
        }
        let a: Vec<Arg> = args.iter().map(|s| arg(s)).collect::<Result<_>>()?;
        let tys: Vec<Ty> = a.iter().map(Arg::ty).collect();
        let (b, n, t, i) = (Out::Bool, Out::Net, Out::Text, |x: i32| Out::Int(x as i64));
        let cmp = |c: Cmp, x: &Net, y: &Net| compare_with(c, x, y).map(Out::Bool);
        let conv = |x: &Net, k: Kind, how: Syntax| convert(x, k, how).map(Out::Net);
        // An integer argument's own type (`smallint`, `integer` or `bigint`) is only known to the caller,
        // and Postgres names it in the error, so a call with one is resolved on its exact types first:
        // `macaddr + integer` is not `macaddr + bigint`, nor `set_masklen(macaddr, smallint)` an integer.
        if a.iter().any(|x| matches!(x, Arg::Int(..))) {
            match (OPERATORS.contains(&op), tys.as_slice()) {
                (true, &[l, r]) => resolve(op, l, r)?,
                (false, _) => resolve_function(op, &tys)?,
                _ => {}
            }
        }
        match (op, a.as_slice()) {
            ("<", [Arg::Net(x), Arg::Net(y)]) => cmp(Cmp::Lt, x, y),
            ("<=", [Arg::Net(x), Arg::Net(y)]) => cmp(Cmp::Le, x, y),
            ("=", [Arg::Net(x), Arg::Net(y)]) => cmp(Cmp::Eq, x, y),
            ("<>", [Arg::Net(x), Arg::Net(y)]) => cmp(Cmp::Ne, x, y),
            (">=", [Arg::Net(x), Arg::Net(y)]) => cmp(Cmp::Ge, x, y),
            (">", [Arg::Net(x), Arg::Net(y)]) => cmp(Cmp::Gt, x, y),
            ("<<", [Arg::Net(x), Arg::Net(y)]) => within(x, y).map(b),
            ("<<=", [Arg::Net(x), Arg::Net(y)]) => within_eq(x, y).map(b),
            (">>", [Arg::Net(x), Arg::Net(y)]) => contains(x, y).map(b),
            (">>=", [Arg::Net(x), Arg::Net(y)]) => contains_eq(x, y).map(b),
            ("&&", [Arg::Net(x), Arg::Net(y)]) => overlaps(x, y).map(b),
            ("&", [Arg::Net(x), Arg::Net(y)]) => and(x, y).map(n),
            ("|", [Arg::Net(x), Arg::Net(y)]) => or(x, y).map(n),
            ("~", [Arg::Net(x)]) => not(x).map(n),
            ("+", [Arg::Net(x), Arg::Int(k, _)]) => add(x, *k).map(n),
            ("+", [Arg::Int(k, _), Arg::Net(x)]) => add_to(*k, x).map(n),
            ("-", [Arg::Net(x), Arg::Int(k, _)]) => sub(x, *k).map(n),
            ("-", [Arg::Net(x), Arg::Net(y)]) => diff(x, y).map(Out::Int),
            ("host", [Arg::Net(x)]) => host(x).map(t),
            ("text", [Arg::Net(x)]) => text(x).map(t),
            ("abbrev", [Arg::Net(x)]) => abbrev(x).map(t),
            ("broadcast", [Arg::Net(x)]) => broadcast(x).map(n),
            ("netmask", [Arg::Net(x)]) => netmask(x).map(n),
            ("hostmask", [Arg::Net(x)]) => hostmask(x).map(n),
            ("network", [Arg::Net(x)]) => network(x).map(n),
            ("masklen", [Arg::Net(x)]) => masklen(x).map(i),
            ("family", [Arg::Net(x)]) => family(x).map(i),
            // the mask length is an integer; a smallint widens to it, a bigint does not narrow
            ("set_masklen", [Arg::Net(x), Arg::Int(k, Ty::Int2 | Ty::Int4)]) => set_masklen(x, *k as i32).map(n),
            ("inet_same_family", [Arg::Net(x), Arg::Net(y)]) => inet_same_family(x, y).map(b),
            ("inet_merge", [Arg::Net(x), Arg::Net(y)]) => inet_merge(x, y).map(n),
            ("to_cidr", [Arg::Net(x)]) => to_cidr(x).map(n),
            ("to_inet", [Arg::Net(x)]) => to_inet(x).map(n),
            ("to_macaddr8", [Arg::Net(x)]) => mac_to_mac8(x).map(n),
            ("to_macaddr", [Arg::Net(x)]) => mac8_to_mac(x).map(n),
            ("fn_inet", [Arg::Net(x)]) => conv(x, Kind::Inet, Syntax::Call),
            ("fn_cidr", [Arg::Net(x)]) => conv(x, Kind::Cidr, Syntax::Call),
            ("fn_macaddr", [Arg::Net(x)]) => conv(x, Kind::Mac, Syntax::Call),
            ("fn_macaddr8", [Arg::Net(x)]) => conv(x, Kind::Mac8, Syntax::Call),
            ("trunc", [Arg::Net(x)]) => trunc(x).map(n),
            ("encode", [Arg::Net(x)]) => Ok(t(encode(x).iter().map(|b| format!("{b:02x}")).collect())),
            ("macaddr8_set7bit", [Arg::Net(x)]) => macaddr8_set7bit(x).map(n),
            // a shape with no function behind it, such as `inet & bigint`: Postgres's message for it
            (_, [_, _]) if OPERATORS.contains(&op) => {
                resolve(op, tys[0], tys[1])?;
                panic!("no such call {op}({}): resolve accepts it, but nothing runs it", args.join(", "))
            }
            _ => no_function(op, &tys),
        }
    }

    /// The result as Postgres shows it: a value's output text (`t` or `f` for a bool), or an error as
    /// `sqlstate message`, with ` | detail` after it if it has one. This is the form of the tables below.
    fn show(op: &str, args: &[&str]) -> String {
        match call(op, args) {
            Ok(Out::Net(x)) => print(&x),
            Ok(Out::Bool(x)) => (if x { "t" } else { "f" }).into(),
            Ok(Out::Int(x)) => x.to_string(),
            Ok(Out::Text(x)) => x,
            Err(e) => match e.info.as_ref().and_then(|v| v.detail.as_ref()) {
                Some(d) => format!("{} {} | {d}", e.code, e.message),
                None => format!("{} {}", e.code, e.message),
            },
        }
    }

    /// Every line of a table of input texts, run as `op`; all the ones that differ, in one failure.
    fn check_in(op: &str, table: &[(&str, &str)]) {
        let bad: Vec<String> = table
            .iter()
            .filter(|(src, want)| show(op, &[src]) != *want)
            .map(|(src, want)| format!("{op} {src:?}: got {:?}, Postgres says {want:?}", show(op, &[src])))
            .collect();
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// The same for operators and functions: a call, its arguments as `type:text`, the answer.
    fn check(table: &[(&str, &[&str], &str)]) {
        let bad: Vec<String> = table
            .iter()
            .filter(|(op, args, want)| show(op, args) != *want)
            .map(|(op, args, want)| {
                format!("{op}({}): got {:?}, Postgres says {want:?}", args.join(", "), show(op, args))
            })
            .collect();
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// The sample value a type name stands for in the typed tables: one of each type, the same ones the
    /// answers were asked of Postgres with.
    fn sample(ty: &str) -> &'static str {
        match ty {
            "inet" => "inet:10.1.2.3/8",
            "cidr" => "cidr:10.0.0.0/8",
            "macaddr" => "macaddr:08:00:2b:01:02:03",
            "macaddr8" => "macaddr8:08:00:2b:ff:fe:01:02:03",
            // the one that has no ff:fe in the middle, so won't convert to a macaddr
            "macaddr8_nofffe" => "macaddr8:08:00:2b:01:02:03:04:05",
            "smallint" => "smallint:5",
            "int" => "int:5",
            "bigint" => "bigint:5",
            _ => panic!("type {ty}"),
        }
    }

    /// The same as `check`, for rows whose arguments are type names standing for their sample value.
    fn check_typed(table: &[(&str, &[&str], &str)]) {
        let bad: Vec<String> = table
            .iter()
            .filter_map(|(op, tys, want)| {
                let args: Vec<&str> = tys.iter().map(|t| sample(t)).collect();
                let got = show(op, &args);
                (got != *want).then(|| format!("{op}({}): got {got:?}, Postgres says {want:?}", tys.join(", ")))
            })
            .collect();
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// The result the way the `pg_diff_file` protocol shows it: `OK<TAB>text`, or
    /// `ERR<TAB>sqlstate<TAB>message<TAB>detail`.
    fn run(op: &str, args: &[&str]) -> String {
        match call(op, args) {
            Ok(Out::Net(x)) => format!("OK\t{}", print(&x)),
            Ok(Out::Bool(x)) => format!("OK\t{}", if x { "t" } else { "f" }),
            Ok(Out::Int(x)) => format!("OK\t{x}"),
            Ok(Out::Text(x)) => format!("OK\t{x}"),
            Err(e) => {
                let detail = e.info.as_ref().and_then(|v| v.detail.clone()).unwrap_or_default();
                format!("ERR\t{}\t{}\t{}", e.code, e.message, detail)
            }
        }
    }

    /// A xorshift generator, so the property tests need no crate and always see the same values.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Mostly values that collide or nest (few bases, every mask length), so orderings get tested
    /// where they are subtle.
    fn rand_net(r: &mut Rng) -> Net {
        let pool4 = [0x0a00_0000u128, 0x0a01_0000, 0xc0a8_0100, 0xffff_ffff, 0];
        let pool6 = [0u128, 1, 0x2001_0db8 << 96, 0xffff << 80 | 0x0a00_0000, u128::MAX];
        match r.below(6) {
            0..=2 => {
                let v6 = r.below(2) == 0;
                let (pool, max) = if v6 { (&pool6, 128) } else { (&pool4, 32) };
                let mut ip = pool[r.below(5) as usize];
                if r.below(2) == 0 {
                    ip ^= 1u128 << r.below(max);
                }
                if r.below(4) == 0 {
                    ip = (r.next() as u128) << 64 | r.next() as u128;
                }
                ip &= Addr::full(v6);
                let a = Addr { v6, bits: r.below(max + 1) as u8, ip };
                match r.below(2) {
                    0 => Net::Inet(a),
                    _ => Net::Cidr(Addr { ip: a.network(), ..a }),
                }
            }
            3 => Net::Mac(r.next() & 0xffff_ffff_ffff),
            4 => Net::Mac(r.below(4) << 40 | r.below(3)),
            _ => Net::Mac8(if r.below(2) == 0 { r.next() } else { r.below(3) << 56 | r.below(3) }),
        }
    }

    #[test]
    fn stored_form_round_trips_and_sorts_like_ord() {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15);
        let mut all: Vec<Net> = (0..4000).map(|_| rand_net(&mut r)).collect();
        for n in &all {
            assert_eq!(decode(&encode(n)).as_ref(), Some(n), "{n:?}");
        }
        for _ in 0..40_000 {
            let (a, b) = (&all[r.below(4000) as usize], &all[r.below(4000) as usize]);
            assert_eq!(encode(a).cmp(&encode(b)), a.cmp(b), "{a:?} {b:?}");
            assert_eq!(a.cmp(b), b.cmp(a).reverse());
            assert_eq!(a.cmp(b).is_eq(), a == b);
        }
        all.sort();
        assert!(all.windows(2).all(|w| encode(&w[0]) <= encode(&w[1])));
    }

    #[test]
    fn decode_refuses_what_encode_never_makes() {
        let good = encode(&parse(Kind::Cidr, "10.1.0.0/16").unwrap());
        assert!(decode(&good).is_some());
        assert!(decode(&[]).is_none());
        assert!(decode(&good[..good.len() - 1]).is_none());
        let mut b = good.clone();
        b[0] = 0x05;
        assert!(decode(&b).is_none());
        // a mask longer than the address
        let mut b = good.clone();
        b[5] = 33;
        assert!(decode(&b).is_none());
        // host bits set in a cidr
        let mut b = good.clone();
        b[9] = 1;
        assert!(decode(&b).is_none());
        // network bits that aren't the address's
        let mut b = good.clone();
        b[2] = 9;
        assert!(decode(&b).is_none());
        assert!(decode(&[TAG_MAC, 1, 2, 3]).is_none());
        assert!(decode(&[TAG_MAC8; 9]).is_some());
    }

    #[test]
    fn printed_values_read_back() {
        let mut r = Rng(42);
        for _ in 0..4000 {
            let n = rand_net(&mut r);
            let back = parse(n.kind(), &print(&n)).unwrap_or_else(|e| panic!("{n:?} -> {} -> {e}", print(&n)));
            assert_eq!(back, n);
        }
    }

    #[test]
    fn ord_is_network_cmp() {
        let ip = |s: &str| parse(Kind::Inet, s).unwrap();
        // v4 before v6; then the network bits; then the mask length; then the whole address
        let shuffled = [
            "255.255.255.255",
            "::",
            "0.0.0.0/1",
            "10.0.0.0/8",
            "10.0.0.0/16",
            "10.0.0.1/16",
            "10.1.0.0/16",
            "11.0.0.0/8",
        ];
        let mut v: Vec<Net> = shuffled.iter().map(|s| ip(s)).collect();
        v.sort();
        let shown: Vec<String> = v.iter().map(print).collect();
        assert_eq!(
            shown,
            [
                "0.0.0.0/1",
                "10.0.0.0/8",
                "10.0.0.0/16",
                "10.0.0.1/16",
                "10.1.0.0/16",
                "11.0.0.0/8",
                "255.255.255.255",
                "::"
            ]
        );
    }

    #[test]
    fn inet_and_cidr_are_equal_values_but_different_kinds() {
        let inet = parse(Kind::Inet, "10.0.0.0/8").unwrap();
        let cidr = parse(Kind::Cidr, "10.0.0.0/8").unwrap();
        // SQL's `=` (the cidr is cast to inet) says equal; `==` and `Ord` keep the kinds apart
        assert_eq!(compare(&inet, &cidr), Ordering::Equal);
        assert_ne!(inet, cidr);
        assert_eq!(inet.cmp(&cidr), Ordering::Less);
        assert_eq!(to_inet(&cidr).unwrap(), inet);
        // and an inet has no more host bits to lose than it had
        assert_eq!(to_cidr(&parse(Kind::Inet, "10.1.2.3/8").unwrap()).unwrap(), cidr);
    }

    #[test]
    fn a_nul_is_not_the_end_of_the_text() {
        for kind in [Kind::Inet, Kind::Cidr, Kind::Mac, Kind::Mac8] {
            assert!(parse(kind, "1.2.3.4\0junk").is_err());
            assert!(parse(kind, "08:00:2b:01:02:03\0").is_err());
        }
    }

    /// `sqlstate message` of a failure, which is what Postgres's answer is checked as.
    fn failed<T>(r: Result<T>) -> String {
        match r {
            Ok(_) => "no error".into(),
            Err(e) => format!("{} {}", e.code, e.message),
        }
    }

    fn net(kind: Kind, s: &str) -> Net {
        parse(kind, s).unwrap()
    }

    // The messages below are Postgres 16.13's. The typed tables check them through `call`, which
    // resolves integer operands itself; these call the functions alone, as a SQL layer that has
    // already picked the function by name would.

    #[test]
    fn a_function_of_the_wrong_type_is_undefined() {
        let (inet, cidr) = (net(Kind::Inet, "1.2.3.4"), net(Kind::Cidr, "1.2.3.0/24"));
        let (mac, mac8) = (net(Kind::Mac, "08:00:2b:01:02:03"), net(Kind::Mac8, "08:00:2b:01:02:03:04:05"));
        assert_eq!(failed(host(&mac)), "42883 function host(macaddr) does not exist");
        assert_eq!(failed(trunc(&inet)), "42883 function trunc(inet) does not exist");
        assert_eq!(failed(macaddr8_set7bit(&cidr)), "42883 function macaddr8_set7bit(cidr) does not exist");
        // every argument's type is named, not just the one at fault
        assert_eq!(
            failed(inet_same_family(&mac, &mac)),
            "42883 function inet_same_family(macaddr, macaddr) does not exist"
        );
        assert_eq!(failed(inet_merge(&inet, &mac)), "42883 function inet_merge(inet, macaddr) does not exist");
        assert_eq!(failed(inet_merge(&mac8, &cidr)), "42883 function inet_merge(macaddr8, cidr) does not exist");
        // a mask length is an integer
        assert_eq!(failed(set_masklen(&mac, 8)), "42883 function set_masklen(macaddr, integer) does not exist");
    }

    #[test]
    fn an_operator_of_the_wrong_types_is_named_as_an_operator() {
        let (inet, cidr) = (net(Kind::Inet, "1.2.3.4"), net(Kind::Cidr, "1.2.3.0/24"));
        let (mac, mac8) = (net(Kind::Mac, "08:00:2b:01:02:03"), net(Kind::Mac8, "08:00:2b:01:02:03:04:05"));
        assert_eq!(failed(and(&inet, &mac)), "42883 operator does not exist: inet & macaddr");
        assert_eq!(failed(or(&mac8, &cidr)), "42883 operator does not exist: macaddr8 | cidr");
        assert_eq!(failed(within(&mac, &inet)), "42883 operator does not exist: macaddr << inet");
        assert_eq!(failed(within_eq(&inet, &mac)), "42883 operator does not exist: inet <<= macaddr");
        assert_eq!(failed(contains(&cidr, &mac8)), "42883 operator does not exist: cidr >> macaddr8");
        assert_eq!(failed(contains_eq(&mac, &mac)), "42883 operator does not exist: macaddr >>= macaddr");
        assert_eq!(failed(overlaps(&mac, &mac8)), "42883 operator does not exist: macaddr && macaddr8");
        // a failed subtraction says `-` and one of a sum says `+`; the integer is a bigint, and its side
        // is the side it was on
        assert_eq!(failed(add(&mac, 5)), "42883 operator does not exist: macaddr + bigint");
        assert_eq!(failed(add_to(5, &mac)), "42883 operator does not exist: bigint + macaddr");
        assert_eq!(failed(sub(&mac8, 5)), "42883 operator does not exist: macaddr8 - bigint");
        assert_eq!(failed(diff(&mac, &mac)), "42883 operator does not exist: macaddr - macaddr");
        assert_eq!(failed(diff(&inet, &mac)), "42883 operator does not exist: inet - macaddr");
        // the valid ones still work, cidr standing in for inet
        assert!(within(&cidr, &net(Kind::Cidr, "1.0.0.0/8")).unwrap());
        assert_eq!(print(&add_to(1, &cidr).unwrap()), "1.2.3.1/24");
    }

    #[test]
    fn a_mac_with_a_mac8_fits_two_operators_so_is_not_unique() {
        let (mac, mac8) = (net(Kind::Mac, "08:00:2b:01:02:03"), net(Kind::Mac8, "08:00:2b:ff:fe:01:02:03"));
        // each converts to the other implicitly, so `&` and `|` and the six comparisons all fit two ways
        assert_eq!(failed(and(&mac, &mac8)), "42725 operator is not unique: macaddr & macaddr8");
        assert_eq!(failed(or(&mac8, &mac)), "42725 operator is not unique: macaddr8 | macaddr");
        for (op, sym) in
            [(Cmp::Eq, "="), (Cmp::Ne, "<>"), (Cmp::Lt, "<"), (Cmp::Le, "<="), (Cmp::Gt, ">"), (Cmp::Ge, ">=")]
        {
            assert_eq!(
                failed(compare_with(op, &mac, &mac8)),
                format!("42725 operator is not unique: macaddr {sym} macaddr8")
            );
            assert_eq!(
                failed(compare_with(op, &mac8, &mac)),
                format!("42725 operator is not unique: macaddr8 {sym} macaddr")
            );
        }
        // the containment operators have no mac version at all, so nothing is ambiguous
        assert_eq!(failed(within(&mac, &mac8)), "42883 operator does not exist: macaddr << macaddr8");
    }

    #[test]
    fn sql_comparison_refuses_what_ord_orders() {
        let (inet, cidr) = (net(Kind::Inet, "10.0.0.0/8"), net(Kind::Cidr, "10.0.0.0/8"));
        let (mac, mac8) = (net(Kind::Mac, "08:00:2b:01:02:03"), net(Kind::Mac8, "08:00:2b:ff:fe:01:02:03"));
        // `Ord` has to put any two values in some order, for sorting ...
        assert_ne!(compare(&inet, &mac), Ordering::Equal);
        assert_ne!(compare(&mac, &mac8), Ordering::Equal);
        // ... and SQL doesn't: no operator, in either order, for any of the six
        for op in [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge] {
            for (a, b) in [(&inet, &mac), (&mac, &inet), (&cidr, &mac8), (&mac8, &cidr)] {
                let e = compare_with(op, a, b).unwrap_err();
                assert_eq!(e.code, "42883");
                assert_eq!(
                    e.message,
                    format!("operator does not exist: {} {} {}", a.ty().name(), op.sym(), b.ty().name())
                );
            }
        }
        // inet with cidr, as before
        assert!(compare_with(Cmp::Eq, &inet, &cidr).unwrap());
        assert!(compare_with(Cmp::Lt, &net(Kind::Inet, "9.0.0.0"), &cidr).unwrap());
        assert!(compare_with(Cmp::Gt, &mac8, &net(Kind::Mac8, "00:00:00:00:00:00:00:00")).unwrap());
    }

    #[test]
    fn an_integer_keeps_its_own_name_in_the_error() {
        let (inet, mac) = (Ty::Net(Kind::Inet), Ty::Net(Kind::Mac));
        // macaddr + 5 is `macaddr + integer` unless the 5 is a bigint; the caller has to say which
        assert_eq!(failed(resolve("+", mac, Ty::Int2)), "42883 operator does not exist: macaddr + smallint");
        assert_eq!(failed(resolve("+", mac, Ty::Int4)), "42883 operator does not exist: macaddr + integer");
        assert_eq!(failed(resolve("+", Ty::Int8, mac)), "42883 operator does not exist: bigint + macaddr");
        // there is inet + bigint and bigint + inet, but only inet - bigint
        assert_eq!(failed(resolve("-", Ty::Int4, inet)), "42883 operator does not exist: integer - inet");
        assert!(resolve("-", inet, Ty::Int2).is_ok());
        assert!(resolve("+", Ty::Int2, Ty::Net(Kind::Cidr)).is_ok());
        // an operator no network type has is none at all
        assert_eq!(failed(resolve("=", inet, Ty::Int4)), "42883 operator does not exist: inet = integer");
        assert_eq!(failed(resolve("&", inet, Ty::Int8)), "42883 operator does not exist: inet & bigint");
        assert_eq!(failed(resolve("+", inet, inet)), "42883 operator does not exist: inet + inet");
        // and the same for functions: a smallint widens to a mask length, a bigint doesn't
        assert_eq!(
            failed(resolve_function("set_masklen", &[mac, Ty::Int2])),
            "42883 function set_masklen(macaddr, smallint) does not exist"
        );
        assert_eq!(
            failed(resolve_function("set_masklen", &[inet, Ty::Int8])),
            "42883 function set_masklen(inet, bigint) does not exist"
        );
        assert!(resolve_function("set_masklen", &[inet, Ty::Int2]).is_ok());
        assert_eq!(failed(resolve_function("host", &[Ty::Int4])), "42883 function host(integer) does not exist");
        assert!(resolve_function("text", &[mac]).is_ok());
    }

    #[test]
    fn a_conversion_is_a_cast_or_a_call() {
        let (inet, cidr) = (net(Kind::Inet, "10.1.2.3/8"), net(Kind::Cidr, "10.0.0.0/8"));
        let (mac, mac8) = (net(Kind::Mac, "08:00:2b:01:02:03"), net(Kind::Mac8, "08:00:2b:ff:fe:01:02:03"));
        // a type converted to itself is a no-op
        for (x, k) in [(&inet, Kind::Inet), (&cidr, Kind::Cidr), (&mac, Kind::Mac), (&mac8, Kind::Mac8)] {
            for how in [Syntax::Cast, Syntax::Call] {
                assert_eq!(convert(x, k, how).as_ref(), Ok(x));
            }
        }
        assert_eq!(mac8_to_mac(&mac), Ok(mac.clone()));
        assert_eq!(mac_to_mac8(&mac8), Ok(mac8.clone()));
        // what is unrelated differs by how it was written
        for (x, k, cast, call) in [
            (&mac, Kind::Inet, "cannot cast type macaddr to inet", "function inet(macaddr) does not exist"),
            (&mac8, Kind::Cidr, "cannot cast type macaddr8 to cidr", "function cidr(macaddr8) does not exist"),
            (&inet, Kind::Mac, "cannot cast type inet to macaddr", "function macaddr(inet) does not exist"),
            (&cidr, Kind::Mac8, "cannot cast type cidr to macaddr8", "function macaddr8(cidr) does not exist"),
        ] {
            assert_eq!(failed(convert(x, k, Syntax::Cast)), format!("42846 {cast}"));
            assert_eq!(failed(convert(x, k, Syntax::Call)), format!("42883 {call}"));
        }
        // `cidr(macaddr)` says cidr, not network
        assert_eq!(failed(to_cidr(&mac)), "42846 cannot cast type macaddr to cidr");
        // only the value that has ff:fe in the middle goes back to a macaddr
        let odd = net(Kind::Mac8, "08:00:2b:01:02:03:04:05");
        for how in [Syntax::Cast, Syntax::Call] {
            assert_eq!(failed(convert(&odd, Kind::Mac, how)), "22003 macaddr8 data out of range to convert to macaddr");
        }
    }

    #[test]
    fn a_macaddr_is_a_macaddr8_where_a_macaddr8_is_wanted() {
        let zero = net(Kind::Mac, "00:00:00:00:00:00");
        // macaddr8_set7bit takes a macaddr by the implicit cast: ff:fe in, then the bit
        assert_eq!(print(&macaddr8_set7bit(&zero).unwrap()), "02:00:00:ff:fe:00:00:00");
        assert_eq!(print(&macaddr8_set7bit(&net(Kind::Mac, "08:00:2b:01:02:03")).unwrap()), "0a:00:2b:ff:fe:01:02:03");
        // text(x) on a mac is a cast to text: the output text
        assert_eq!(text(&net(Kind::Mac, "08:00:2b:01:02:03")).unwrap(), "08:00:2b:01:02:03");
        assert_eq!(text(&net(Kind::Mac8, "08:00:2b:ff:fe:01:02:03")).unwrap(), "08:00:2b:ff:fe:01:02:03");
        // while text() of an inet is its own function, which keeps the mask
        assert_eq!(text(&net(Kind::Inet, "1.2.3.4")).unwrap(), "1.2.3.4/32");
    }

    /// The differential test against a real Postgres, run by hand (it is ignored otherwise):
    /// `NET_PGDIFF_IN=cases NET_PGDIFF_OUT=answers cargo test net::tests::pg_diff_file -- --ignored`.
    /// A line of `cases` is `op`, then each argument in hex, tab separated; a line of `answers` is
    /// the hex of what `run` says. A script feeds the same cases to Postgres and compares.
    #[test]
    #[ignore]
    fn pg_diff_file() {
        use std::fmt::Write;
        let (Ok(inp), Ok(out)) = (std::env::var("NET_PGDIFF_IN"), std::env::var("NET_PGDIFF_OUT")) else { return };
        let unhex = |h: &str| -> String {
            let b: Vec<u8> = (0..h.len() / 2).map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap()).collect();
            String::from_utf8(b).unwrap()
        };
        let mut answers = String::new();
        for line in std::fs::read_to_string(inp).unwrap().lines() {
            let mut parts = line.split('\t');
            let op = parts.next().unwrap().to_string();
            let args: Vec<String> = parts.map(unhex).collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            // a panic here is a bug too, but one case's must not hide the rest
            let got = std::panic::catch_unwind(|| run(&op, &args)).unwrap_or_else(|_| "PANIC".into());
            for b in got.bytes() {
                write!(answers, "{b:02x}").unwrap();
            }
            answers.push('\n');
        }
        std::fs::write(out, answers).unwrap();
    }

    #[test]
    fn inet_input_and_output() {
        check_in("inet", INET_IN);
    }

    #[test]
    fn cidr_input_and_output() {
        check_in("cidr", CIDR_IN);
    }

    #[test]
    fn macaddr_input_and_output() {
        check_in("macaddr", MACADDR_IN);
    }

    #[test]
    fn macaddr8_input_and_output() {
        check_in("macaddr8", MACADDR8_IN);
    }

    #[test]
    fn comparisons() {
        check(COMPARE);
    }

    #[test]
    fn containment() {
        check(CONTAIN);
    }

    #[test]
    fn bit_operations() {
        check(BITS);
    }

    #[test]
    fn arithmetic() {
        check(ARITH);
    }

    #[test]
    fn functions_and_conversions() {
        check(FUNCS);
    }

    #[test]
    fn operators_on_the_wrong_types() {
        check_typed(OPERATOR_TYPES);
    }

    #[test]
    fn functions_on_the_wrong_types() {
        check_typed(FUNCTION_TYPES);
    }

    #[test]
    fn conversions_between_the_types() {
        check_typed(CONVERSIONS);
    }

    // What Postgres 16.13 answered for each of these (psql, `format('%s', ...)`, errors by SQLSTATE and
    // message); `check_in` and `check` compare every line.

    const INET_IN: &[(&str, &str)] = &[
        ("192.168.1.5", "192.168.1.5"),
        ("192.168.1.5/24", "192.168.1.5/24"),
        ("192.168.1.5/32", "192.168.1.5"),
        ("192.168.1.5/0", "192.168.1.5/0"),
        ("192.168.1.5/33", "22P02 invalid input syntax for type inet: \"192.168.1.5/33\""),
        ("0.0.0.0/0", "0.0.0.0/0"),
        ("255.255.255.255", "255.255.255.255"),
        ("10/8", "10.0.0.0/8"),
        ("10.1/16", "10.1.0.0/16"),
        ("10.1.2/24", "10.1.2.0/24"),
        ("10", "22P02 invalid input syntax for type inet: \"10\""),
        ("10.1", "22P02 invalid input syntax for type inet: \"10.1\""),
        ("010.1.1.1", "10.1.1.1"),
        ("1.2.3.4.", "1.2.3.4"),
        ("1.2.3.4/", "22P02 invalid input syntax for type inet: \"1.2.3.4/\""),
        ("256.1.1.1", "22P02 invalid input syntax for type inet: \"256.1.1.1\""),
        ("1.2.3", "22P02 invalid input syntax for type inet: \"1.2.3\""),
        ("1.2.3.4/024", "1.2.3.4/24"),
        ("1.2.3.4/4294967296", "1.2.3.4/0"),
        ("1.2.3./24", "1.2.3.0/24"),
        ("1.2/24", "22P02 invalid input syntax for type inet: \"1.2/24\""),
        ("214/29", "22P02 invalid input syntax for type inet: \"214/29\""),
        ("1.2.3.4/24/8", "22P02 invalid input syntax for type inet: \"1.2.3.4/24/8\""),
        ("::1", "::1"),
        ("::", "::"),
        ("::/0", "::/0"),
        ("2001:db8::1/64", "2001:db8::1/64"),
        ("2001:DB8::ABCD", "2001:db8::abcd"),
        ("2001:0db8:0000:0000:0000:0000:0000:0001", "2001:db8::1"),
        ("::ffff:1.2.3.4", "::ffff:1.2.3.4"),
        ("::ffff:1.2.3.4/120", "::ffff:1.2.3.4/120"),
        ("::1.2.3.4", "::1.2.3.4"),
        ("::2", "::2"),
        ("::1:2", "::0.1.0.2"),
        ("::ffff:1:2", "::ffff:0.1.0.2"),
        ("1:0:0:2:0:0:0:3", "1:0:0:2::3"),
        ("1:0:0:0:2::3", "1::2:0:0:3"),
        ("1:0:2:0:0:3::", "1:0:2::3:0:0"),
        ("1::2::3", "22P02 invalid input syntax for type inet: \"1::2::3\""),
        (":::", "22P02 invalid input syntax for type inet: \":::\""),
        ("1:2:3:4:5:6:7:8:9", "22P02 invalid input syntax for type inet: \"1:2:3:4:5:6:7:8:9\""),
        ("12345::", "22P02 invalid input syntax for type inet: \"12345::\""),
        ("::1/129", "22P02 invalid input syntax for type inet: \"::1/129\""),
        ("::1/064", "22P02 invalid input syntax for type inet: \"::1/064\""),
        ("::ffff:010.0.0.1", "22P02 invalid input syntax for type inet: \"::ffff:010.0.0.1\""),
        ("::1..2", "::1.0.2.0"),
        ("::1.2", "::1.2.0.0"),
        ("1:2:3:4:5:6:1.2.3.4", "1:2:3:4:5:6:102:304"),
        ("1:2:3:4:5:6:7:1.2.3.4", "22P02 invalid input syntax for type inet: \"1:2:3:4:5:6:7:1.2.3.4\""),
        ("::1%eth0", "22P02 invalid input syntax for type inet: \"::1%eth0\""),
        ("[::1]", "22P02 invalid input syntax for type inet: \"[::1]\""),
        (" 1.2.3.4", "22P02 invalid input syntax for type inet: \" 1.2.3.4\""),
        ("1.2.3.4 ", "22P02 invalid input syntax for type inet: \"1.2.3.4 \""),
        ("x", "22P02 invalid input syntax for type inet: \"x\""),
        ("", "22P02 invalid input syntax for type inet: \"\""),
        ("0x0a.1.1.1", "22P02 invalid input syntax for type inet: \"0x0a.1.1.1\""),
    ];

    const CIDR_IN: &[(&str, &str)] = &[
        ("10.0.0.0/8", "10.0.0.0/8"),
        ("10.1.2.3/8", "22P02 invalid cidr value: \"10.1.2.3/8\" | Value has bits set to right of mask."),
        ("10.1.2.3", "10.1.2.3/32"),
        ("10", "10.0.0.0/8"),
        ("10.1", "10.1.0.0/16"),
        ("10.1.2", "10.1.2.0/24"),
        ("192.168", "192.168.0.0/24"),
        ("192.168.1", "192.168.1.0/24"),
        ("224", "224.0.0.0/4"),
        ("225", "225.0.0.0/8"),
        ("240", "240.0.0.0/32"),
        ("0", "0.0.0.0/8"),
        ("127", "127.0.0.0/8"),
        ("128", "128.0.0.0/16"),
        ("0x0a", "10.0.0.0/8"),
        ("0xc0a8", "192.168.0.0/24"),
        ("0xc0a80101", "192.168.1.1/32"),
        ("0xa", "160.0.0.0/16"),
        ("192.168.1.0/24", "192.168.1.0/24"),
        ("192.168.1.128/25", "192.168.1.128/25"),
        ("192.168.1.128/24", "22P02 invalid cidr value: \"192.168.1.128/24\" | Value has bits set to right of mask."),
        ("0.0.0.0/0", "0.0.0.0/0"),
        ("1.2.3.4/32", "1.2.3.4/32"),
        ("10.0.0.0/33", "22P02 invalid input syntax for type cidr: \"10.0.0.0/33\""),
        ("8.8.8.8/31", "8.8.8.8/31"),
        ("2001:db8::/32", "2001:db8::/32"),
        ("2001:db8::1/64", "22P02 invalid cidr value: \"2001:db8::1/64\" | Value has bits set to right of mask."),
        ("::/0", "::/0"),
        ("::1/128", "::1/128"),
        ("::ffff:1.2.3.0/120", "::ffff:1.2.3.0/120"),
        (
            "::ffff:1.2.3.4/120",
            "22P02 invalid cidr value: \"::ffff:1.2.3.4/120\" | Value has bits set to right of mask.",
        ),
        ("2001:db8:0:1::/64", "2001:db8:0:1::/64"),
        ("fe80::/10", "fe80::/10"),
        ("x", "22P02 invalid input syntax for type cidr: \"x\""),
        ("", "22P02 invalid input syntax for type cidr: \"\""),
        ("10.0.0.0/8 ", "22P02 invalid input syntax for type cidr: \"10.0.0.0/8 \""),
        ("10/8", "10.0.0.0/8"),
        ("10.1.0.0/16", "10.1.0.0/16"),
        ("172.16.0.0/12", "172.16.0.0/12"),
        ("172.16.0.0/11", "22P02 invalid cidr value: \"172.16.0.0/11\" | Value has bits set to right of mask."),
    ];

    const MACADDR_IN: &[(&str, &str)] = &[
        ("08:00:2b:01:02:03", "08:00:2b:01:02:03"),
        ("08-00-2b-01-02-03", "08:00:2b:01:02:03"),
        ("08002b:010203", "08:00:2b:01:02:03"),
        ("08002b-010203", "08:00:2b:01:02:03"),
        ("0800.2b01.0203", "08:00:2b:01:02:03"),
        ("0800-2b01-0203", "08:00:2b:01:02:03"),
        ("08002b010203", "08:00:2b:01:02:03"),
        ("08:00:2B:01:02:03", "08:00:2b:01:02:03"),
        ("8:0:2b:1:2:3", "08:00:2b:01:02:03"),
        (" 08:00:2b:01:02:03 ", "08:00:2b:01:02:03"),
        ("0x8:0x0:2b:1:2:3", "08:00:2b:01:02:03"),
        ("+8:0:2b:1:2:3", "08:00:2b:01:02:03"),
        ("-1:0:0:0:0:0", "22003 invalid octet value in \"macaddr\" value: \"-1:0:0:0:0:0\""),
        ("100:0:0:0:0:0", "22003 invalid octet value in \"macaddr\" value: \"100:0:0:0:0:0\""),
        ("08:00:2b:01:02", "22P02 invalid input syntax for type macaddr: \"08:00:2b:01:02\""),
        ("08:00:2b:01:02:03:04", "22P02 invalid input syntax for type macaddr: \"08:00:2b:01:02:03:04\""),
        ("08.00.2b.01.02.03", "22P02 invalid input syntax for type macaddr: \"08.00.2b.01.02.03\""),
        ("g8:00:2b:01:02:03", "22P02 invalid input syntax for type macaddr: \"g8:00:2b:01:02:03\""),
        ("", "22P02 invalid input syntax for type macaddr: \"\""),
        ("08:00:2b:01:02:03x", "22P02 invalid input syntax for type macaddr: \"08:00:2b:01:02:03x\""),
        ("08002b0102030", "22P02 invalid input syntax for type macaddr: \"08002b0102030\""),
        ("0800 2b01 0203", "08:00:2b:01:02:03"),
        ("08:00:2b:01:02:0", "08:00:2b:01:02:00"),
        ("08-00-2b:01-02-03", "22P02 invalid input syntax for type macaddr: \"08-00-2b:01-02-03\""),
        ("ff:ff:ff:ff:ff:ff", "ff:ff:ff:ff:ff:ff"),
        ("0:0:0:0:0:0", "00:00:00:00:00:00"),
        ("1: 2: 3: 4: 5: 6", "01:02:03:04:05:06"),
        ("0x1:2:3:4:5:ffffffff", "22003 invalid octet value in \"macaddr\" value: \"0x1:2:3:4:5:ffffffff\""),
        ("ffffffffff:2:3:4:5:6", "22003 invalid octet value in \"macaddr\" value: \"ffffffffff:2:3:4:5:6\""),
        ("100000001:2:3:4:5:6", "01:02:03:04:05:06"),
        ("08002b01020", "08:00:2b:01:02:00"),
        ("08:00:2b:01:02:03 x", "22P02 invalid input syntax for type macaddr: \"08:00:2b:01:02:03 x\""),
        ("8 0 2b 1 2 3", "08:00:2b:01:02:03"),
    ];

    const MACADDR8_IN: &[(&str, &str)] = &[
        ("08:00:2b:01:02:03:04:05", "08:00:2b:01:02:03:04:05"),
        ("08002b0102030405", "08:00:2b:01:02:03:04:05"),
        ("08:00:2b:01:02:03", "08:00:2b:ff:fe:01:02:03"),
        ("08-00-2b-01-02-03-04-05", "08:00:2b:01:02:03:04:05"),
        ("0800.2b01.0203.0405", "08:00:2b:01:02:03:04:05"),
        ("08002b01:02030405", "08:00:2b:01:02:03:04:05"),
        ("08002b:0102030405", "08:00:2b:01:02:03:04:05"),
        ("08:00-2b:01:02:03:04:05", "22P02 invalid input syntax for type macaddr8: \"08:00-2b:01:02:03:04:05\""),
        ("08:00:2b:01:02:03:04", "22P02 invalid input syntax for type macaddr8: \"08:00:2b:01:02:03:04\""),
        (" 08:00:2b:01:02:03 ", "08:00:2b:ff:fe:01:02:03"),
        ("08:00:2b:01:02:03x", "08:00:2b:ff:fe:01:02:03"),
        ("08:00:2b:01:02:03 x", "22P02 invalid input syntax for type macaddr8: \"08:00:2b:01:02:03 x\""),
        ("08:00:2B:01:02:03:04:05", "08:00:2b:01:02:03:04:05"),
        ("", "22P02 invalid input syntax for type macaddr8: \"\""),
        ("zz:00:2b:01:02:03", "22P02 invalid input syntax for type macaddr8: \"zz:00:2b:01:02:03\""),
        ("08:00:2b:01:02:03:04:05:06", "22P02 invalid input syntax for type macaddr8: \"08:00:2b:01:02:03:04:05:06\""),
        ("0:0:0:0:0:0", "22P02 invalid input syntax for type macaddr8: \"0:0:0:0:0:0\""),
        ("00:00:00:00:00:00", "00:00:00:ff:fe:00:00:00"),
        ("ff:ff:ff:ff:ff:ff", "ff:ff:ff:ff:fe:ff:ff:ff"),
        ("08.00.2b.01.02.03", "08:00:2b:ff:fe:01:02:03"),
        ("0800.2b.01.02.03", "08:00:2b:ff:fe:01:02:03"),
        ("08:00:2b:01:02:03:", "08:00:2b:ff:fe:01:02:03"),
        ("08:00:2b::01:02:03", "22P02 invalid input syntax for type macaddr8: \"08:00:2b::01:02:03\""),
        ("08:00:2b:01:02:03:04:0", "22P02 invalid input syntax for type macaddr8: \"08:00:2b:01:02:03:04:0\""),
        ("8:0:2b:1:2:3", "22P02 invalid input syntax for type macaddr8: \"8:0:2b:1:2:3\""),
        ("08002b010203040506", "22P02 invalid input syntax for type macaddr8: \"08002b010203040506\""),
        ("08002b01020304", "22P02 invalid input syntax for type macaddr8: \"08002b01020304\""),
        ("0800-2b01-0203-0405", "08:00:2b:01:02:03:04:05"),
        ("08002b.0102.03.04.05", "08:00:2b:01:02:03:04:05"),
    ];

    const COMPARE: &[(&str, &[&str], &str)] = &[
        ("<", &["inet:10.0.0.0/8", "inet:10.0.0.0/16"], "t"),
        ("<", &["inet:10.0.0.1/8", "inet:10.0.0.0/8"], "f"),
        ("<", &["inet:255.255.255.255", "inet:::"], "t"),
        ("<", &["inet:::1", "inet:1.2.3.4"], "f"),
        ("<", &["inet:1.2.3.4", "inet:1.2.3.4/32"], "f"),
        ("<", &["inet:192.168.1.5/24", "cidr:192.168.1.0/24"], "f"),
        ("<", &["inet:192.168.1.0/24", "cidr:192.168.1.0/24"], "f"),
        ("<", &["cidr:10.0.0.0/8", "cidr:10.0.0.0/9"], "t"),
        ("<", &["macaddr:08:00:2b:01:02:03", "macaddr:08:00:2b:01:02:04"], "t"),
        ("<", &["macaddr:ff:00:00:00:00:00", "macaddr:00:ff:ff:ff:ff:ff"], "f"),
        ("<", &["macaddr8:08:00:2b:01:02:03:04:05", "macaddr8:08:00:2b:01:02:03:04:05"], "f"),
        ("<", &["macaddr8:00:00:00:00:00:00:00:01", "macaddr8:00:00:00:00:00:00:01:00"], "t"),
        ("<=", &["inet:10.0.0.0/8", "inet:10.0.0.0/16"], "t"),
        ("<=", &["inet:10.0.0.1/8", "inet:10.0.0.0/8"], "f"),
        ("<=", &["inet:255.255.255.255", "inet:::"], "t"),
        ("<=", &["inet:::1", "inet:1.2.3.4"], "f"),
        ("<=", &["inet:1.2.3.4", "inet:1.2.3.4/32"], "t"),
        ("<=", &["inet:192.168.1.5/24", "cidr:192.168.1.0/24"], "f"),
        ("<=", &["inet:192.168.1.0/24", "cidr:192.168.1.0/24"], "t"),
        ("<=", &["cidr:10.0.0.0/8", "cidr:10.0.0.0/9"], "t"),
        ("<=", &["macaddr:08:00:2b:01:02:03", "macaddr:08:00:2b:01:02:04"], "t"),
        ("<=", &["macaddr:ff:00:00:00:00:00", "macaddr:00:ff:ff:ff:ff:ff"], "f"),
        ("<=", &["macaddr8:08:00:2b:01:02:03:04:05", "macaddr8:08:00:2b:01:02:03:04:05"], "t"),
        ("<=", &["macaddr8:00:00:00:00:00:00:00:01", "macaddr8:00:00:00:00:00:00:01:00"], "t"),
        ("=", &["inet:10.0.0.0/8", "inet:10.0.0.0/16"], "f"),
        ("=", &["inet:10.0.0.1/8", "inet:10.0.0.0/8"], "f"),
        ("=", &["inet:255.255.255.255", "inet:::"], "f"),
        ("=", &["inet:::1", "inet:1.2.3.4"], "f"),
        ("=", &["inet:1.2.3.4", "inet:1.2.3.4/32"], "t"),
        ("=", &["inet:192.168.1.5/24", "cidr:192.168.1.0/24"], "f"),
        ("=", &["inet:192.168.1.0/24", "cidr:192.168.1.0/24"], "t"),
        ("=", &["cidr:10.0.0.0/8", "cidr:10.0.0.0/9"], "f"),
        ("=", &["macaddr:08:00:2b:01:02:03", "macaddr:08:00:2b:01:02:04"], "f"),
        ("=", &["macaddr:ff:00:00:00:00:00", "macaddr:00:ff:ff:ff:ff:ff"], "f"),
        ("=", &["macaddr8:08:00:2b:01:02:03:04:05", "macaddr8:08:00:2b:01:02:03:04:05"], "t"),
        ("=", &["macaddr8:00:00:00:00:00:00:00:01", "macaddr8:00:00:00:00:00:00:01:00"], "f"),
        ("<>", &["inet:10.0.0.0/8", "inet:10.0.0.0/16"], "t"),
        ("<>", &["inet:10.0.0.1/8", "inet:10.0.0.0/8"], "t"),
        ("<>", &["inet:255.255.255.255", "inet:::"], "t"),
        ("<>", &["inet:::1", "inet:1.2.3.4"], "t"),
        ("<>", &["inet:1.2.3.4", "inet:1.2.3.4/32"], "f"),
        ("<>", &["inet:192.168.1.5/24", "cidr:192.168.1.0/24"], "t"),
        ("<>", &["inet:192.168.1.0/24", "cidr:192.168.1.0/24"], "f"),
        ("<>", &["cidr:10.0.0.0/8", "cidr:10.0.0.0/9"], "t"),
        ("<>", &["macaddr:08:00:2b:01:02:03", "macaddr:08:00:2b:01:02:04"], "t"),
        ("<>", &["macaddr:ff:00:00:00:00:00", "macaddr:00:ff:ff:ff:ff:ff"], "t"),
        ("<>", &["macaddr8:08:00:2b:01:02:03:04:05", "macaddr8:08:00:2b:01:02:03:04:05"], "f"),
        ("<>", &["macaddr8:00:00:00:00:00:00:00:01", "macaddr8:00:00:00:00:00:00:01:00"], "t"),
        (">=", &["inet:10.0.0.0/8", "inet:10.0.0.0/16"], "f"),
        (">=", &["inet:10.0.0.1/8", "inet:10.0.0.0/8"], "t"),
        (">=", &["inet:255.255.255.255", "inet:::"], "f"),
        (">=", &["inet:::1", "inet:1.2.3.4"], "t"),
        (">=", &["inet:1.2.3.4", "inet:1.2.3.4/32"], "t"),
        (">=", &["inet:192.168.1.5/24", "cidr:192.168.1.0/24"], "t"),
        (">=", &["inet:192.168.1.0/24", "cidr:192.168.1.0/24"], "t"),
        (">=", &["cidr:10.0.0.0/8", "cidr:10.0.0.0/9"], "f"),
        (">=", &["macaddr:08:00:2b:01:02:03", "macaddr:08:00:2b:01:02:04"], "f"),
        (">=", &["macaddr:ff:00:00:00:00:00", "macaddr:00:ff:ff:ff:ff:ff"], "t"),
        (">=", &["macaddr8:08:00:2b:01:02:03:04:05", "macaddr8:08:00:2b:01:02:03:04:05"], "t"),
        (">=", &["macaddr8:00:00:00:00:00:00:00:01", "macaddr8:00:00:00:00:00:00:01:00"], "f"),
        (">", &["inet:10.0.0.0/8", "inet:10.0.0.0/16"], "f"),
        (">", &["inet:10.0.0.1/8", "inet:10.0.0.0/8"], "t"),
        (">", &["inet:255.255.255.255", "inet:::"], "f"),
        (">", &["inet:::1", "inet:1.2.3.4"], "t"),
        (">", &["inet:1.2.3.4", "inet:1.2.3.4/32"], "f"),
        (">", &["inet:192.168.1.5/24", "cidr:192.168.1.0/24"], "t"),
        (">", &["inet:192.168.1.0/24", "cidr:192.168.1.0/24"], "f"),
        (">", &["cidr:10.0.0.0/8", "cidr:10.0.0.0/9"], "f"),
        (">", &["macaddr:08:00:2b:01:02:03", "macaddr:08:00:2b:01:02:04"], "f"),
        (">", &["macaddr:ff:00:00:00:00:00", "macaddr:00:ff:ff:ff:ff:ff"], "t"),
        (">", &["macaddr8:08:00:2b:01:02:03:04:05", "macaddr8:08:00:2b:01:02:03:04:05"], "f"),
        (">", &["macaddr8:00:00:00:00:00:00:00:01", "macaddr8:00:00:00:00:00:00:01:00"], "f"),
    ];

    const CONTAIN: &[(&str, &[&str], &str)] = &[
        ("<<", &["inet:192.168.1.5/24", "inet:192.168.1.0/24"], "f"),
        ("<<", &["inet:192.168.1.5/32", "inet:192.168.1.0/24"], "t"),
        ("<<", &["inet:192.168.1.0/24", "inet:192.168.1.5/32"], "f"),
        ("<<", &["inet:10.1.0.0/16", "inet:10.0.0.0/8"], "t"),
        ("<<", &["cidr:10.1.0.0/16", "cidr:10.0.0.0/8"], "t"),
        ("<<", &["inet:10.0.0.0/8", "inet:10.0.0.0/8"], "f"),
        ("<<", &["inet:11.0.0.0/8", "inet:10.0.0.0/8"], "f"),
        ("<<", &["inet:::1", "inet:::/0"], "t"),
        ("<<=", &["inet:192.168.1.5/24", "inet:192.168.1.0/24"], "t"),
        ("<<=", &["inet:192.168.1.5/32", "inet:192.168.1.0/24"], "t"),
        ("<<=", &["inet:192.168.1.0/24", "inet:192.168.1.5/32"], "f"),
        ("<<=", &["inet:10.1.0.0/16", "inet:10.0.0.0/8"], "t"),
        ("<<=", &["cidr:10.1.0.0/16", "cidr:10.0.0.0/8"], "t"),
        ("<<=", &["inet:10.0.0.0/8", "inet:10.0.0.0/8"], "t"),
        ("<<=", &["inet:11.0.0.0/8", "inet:10.0.0.0/8"], "f"),
        ("<<=", &["inet:::1", "inet:::/0"], "t"),
        (">>", &["inet:192.168.1.5/24", "inet:192.168.1.0/24"], "f"),
        (">>", &["inet:192.168.1.5/32", "inet:192.168.1.0/24"], "f"),
        (">>", &["inet:192.168.1.0/24", "inet:192.168.1.5/32"], "t"),
        (">>", &["inet:10.1.0.0/16", "inet:10.0.0.0/8"], "f"),
        (">>", &["cidr:10.1.0.0/16", "cidr:10.0.0.0/8"], "f"),
        (">>", &["inet:10.0.0.0/8", "inet:10.0.0.0/8"], "f"),
        (">>", &["inet:11.0.0.0/8", "inet:10.0.0.0/8"], "f"),
        (">>", &["inet:::1", "inet:::/0"], "f"),
        (">>=", &["inet:192.168.1.5/24", "inet:192.168.1.0/24"], "t"),
        (">>=", &["inet:192.168.1.5/32", "inet:192.168.1.0/24"], "f"),
        (">>=", &["inet:192.168.1.0/24", "inet:192.168.1.5/32"], "t"),
        (">>=", &["inet:10.1.0.0/16", "inet:10.0.0.0/8"], "f"),
        (">>=", &["cidr:10.1.0.0/16", "cidr:10.0.0.0/8"], "f"),
        (">>=", &["inet:10.0.0.0/8", "inet:10.0.0.0/8"], "t"),
        (">>=", &["inet:11.0.0.0/8", "inet:10.0.0.0/8"], "f"),
        (">>=", &["inet:::1", "inet:::/0"], "f"),
        ("&&", &["inet:192.168.1.5/24", "inet:192.168.1.0/24"], "t"),
        ("&&", &["inet:192.168.1.5/32", "inet:192.168.1.0/24"], "t"),
        ("&&", &["inet:192.168.1.0/24", "inet:192.168.1.5/32"], "t"),
        ("&&", &["inet:10.1.0.0/16", "inet:10.0.0.0/8"], "t"),
        ("&&", &["cidr:10.1.0.0/16", "cidr:10.0.0.0/8"], "t"),
        ("&&", &["inet:10.0.0.0/8", "inet:10.0.0.0/8"], "t"),
        ("&&", &["inet:11.0.0.0/8", "inet:10.0.0.0/8"], "f"),
        ("&&", &["inet:::1", "inet:::/0"], "t"),
    ];

    const BITS: &[(&str, &[&str], &str)] = &[
        ("&", &["inet:192.168.1.6", "inet:0.0.0.255"], "0.0.0.6"),
        ("|", &["inet:192.168.1.6", "inet:0.0.0.255"], "192.168.1.255"),
        ("&", &["inet:1.2.3.4/24", "inet:255.0.255.0/28"], "1.0.3.0/28"),
        ("|", &["inet:1.2.3.4/24", "inet:255.0.255.0/28"], "255.2.255.4/28"),
        ("&", &["inet:::ffff", "inet:::f0f0"], "::f0f0"),
        ("|", &["inet:::ffff", "inet:::f0f0"], "::ffff"),
        ("&", &["inet:1.2.3.4", "inet:::1"], "22023 cannot AND inet values of different sizes"),
        ("|", &["inet:1.2.3.4", "inet:::1"], "22023 cannot OR inet values of different sizes"),
        ("&", &["cidr:10.0.0.0/8", "cidr:0.255.0.0/16"], "0.0.0.0/16"),
        ("|", &["cidr:10.0.0.0/8", "cidr:0.255.0.0/16"], "10.255.0.0/16"),
        ("&", &["macaddr:08:00:2b:01:02:03", "macaddr:ff:00:ff:00:ff:00"], "08:00:2b:00:02:00"),
        ("|", &["macaddr:08:00:2b:01:02:03", "macaddr:ff:00:ff:00:ff:00"], "ff:00:ff:01:ff:03"),
        ("&", &["macaddr8:08:00:2b:01:02:03:04:05", "macaddr8:f0:f0:f0:f0:0f:0f:0f:0f"], "00:00:20:00:02:03:04:05"),
        ("|", &["macaddr8:08:00:2b:01:02:03:04:05", "macaddr8:f0:f0:f0:f0:0f:0f:0f:0f"], "f8:f0:fb:f1:0f:0f:0f:0f"),
        ("~", &["inet:192.168.1.6"], "63.87.254.249"),
        ("~", &["inet:1.2.3.4/24"], "254.253.252.251/24"),
        ("~", &["inet:::1"], "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe"),
        ("~", &["cidr:10.0.0.0/8"], "245.255.255.255/8"),
        ("~", &["macaddr:08:00:2b:01:02:03"], "f7:ff:d4:fe:fd:fc"),
        ("~", &["macaddr8:08:00:2b:01:02:03:04:05"], "f7:ff:d4:fe:fd:fc:fb:fa"),
        ("~", &["inet:0.0.0.0/0"], "255.255.255.255/0"),
        ("~", &["inet:255.255.255.255"], "0.0.0.0"),
    ];

    const ARITH: &[(&str, &[&str], &str)] = &[
        ("+", &["inet:192.168.1.6", "bigint:25"], "192.168.1.31"),
        ("+", &["bigint:25", "inet:192.168.1.6"], "192.168.1.31"),
        ("-", &["inet:192.168.1.6", "bigint:25"], "192.168.0.237"),
        ("+", &["inet:192.168.1.6/24", "bigint:25"], "192.168.1.31/24"),
        ("+", &["bigint:25", "inet:192.168.1.6/24"], "192.168.1.31/24"),
        ("-", &["inet:192.168.1.6/24", "bigint:25"], "192.168.0.237/24"),
        ("+", &["inet:192.168.1.6", "bigint:-1"], "192.168.1.5"),
        ("+", &["bigint:-1", "inet:192.168.1.6"], "192.168.1.5"),
        ("-", &["inet:192.168.1.6", "bigint:-1"], "192.168.1.7"),
        ("+", &["inet:255.255.255.255", "bigint:1"], "22003 result is out of range"),
        ("+", &["bigint:1", "inet:255.255.255.255"], "22003 result is out of range"),
        ("-", &["inet:255.255.255.255", "bigint:1"], "255.255.255.254"),
        ("+", &["inet:0.0.0.0", "bigint:-1"], "22003 result is out of range"),
        ("+", &["bigint:-1", "inet:0.0.0.0"], "22003 result is out of range"),
        ("-", &["inet:0.0.0.0", "bigint:-1"], "0.0.0.1"),
        ("+", &["inet:0.0.0.0", "bigint:4294967295"], "255.255.255.255"),
        ("+", &["bigint:4294967295", "inet:0.0.0.0"], "255.255.255.255"),
        ("-", &["inet:0.0.0.0", "bigint:4294967295"], "22003 result is out of range"),
        ("+", &["inet:0.0.0.0", "bigint:4294967296"], "22003 result is out of range"),
        ("+", &["bigint:4294967296", "inet:0.0.0.0"], "22003 result is out of range"),
        ("-", &["inet:0.0.0.0", "bigint:4294967296"], "22003 result is out of range"),
        ("+", &["inet:::", "bigint:9223372036854775807"], "::7fff:ffff:ffff:ffff"),
        ("+", &["bigint:9223372036854775807", "inet:::"], "::7fff:ffff:ffff:ffff"),
        ("-", &["inet:::", "bigint:9223372036854775807"], "22003 result is out of range"),
        ("+", &["inet:::", "bigint:-9223372036854775808"], "22003 result is out of range"),
        ("+", &["bigint:-9223372036854775808", "inet:::"], "22003 result is out of range"),
        ("-", &["inet:::", "bigint:-9223372036854775808"], "22003 result is out of range"),
        ("+", &["inet:ffff::", "bigint:-9223372036854775808"], "fffe:ffff:ffff:ffff:8000::"),
        ("+", &["bigint:-9223372036854775808", "inet:ffff::"], "fffe:ffff:ffff:ffff:8000::"),
        ("-", &["inet:ffff::", "bigint:-9223372036854775808"], "fffe:ffff:ffff:ffff:8000::"),
        ("+", &["inet:::ffff:ffff:ffff:ffff", "bigint:1"], "0:0:0:1::"),
        ("+", &["bigint:1", "inet:::ffff:ffff:ffff:ffff"], "0:0:0:1::"),
        ("-", &["inet:::ffff:ffff:ffff:ffff", "bigint:1"], "::ffff:ffff:ffff:fffe"),
        ("+", &["inet:ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", "bigint:1"], "22003 result is out of range"),
        ("+", &["bigint:1", "inet:ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"], "22003 result is out of range"),
        ("-", &["inet:ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", "bigint:1"], "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe"),
        ("+", &["cidr:10.0.0.0/8", "bigint:256"], "10.0.1.0/8"),
        ("+", &["bigint:256", "cidr:10.0.0.0/8"], "10.0.1.0/8"),
        ("-", &["cidr:10.0.0.0/8", "bigint:256"], "9.255.255.0/8"),
        ("-", &["inet:192.168.1.43", "inet:192.168.1.19"], "24"),
        ("-", &["inet:192.168.1.19", "inet:192.168.1.43"], "-24"),
        ("-", &["inet:::", "inet:ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"], "1"),
        ("-", &["inet:8000::", "inet:::"], "22003 result is out of range"),
        ("-", &["inet:::8000:0:0:0", "inet:::"], "22003 result is out of range"),
        ("-", &["inet:::8000:0:0:1", "inet:::1"], "22003 result is out of range"),
        ("-", &["inet:1.2.3.4", "inet:::1"], "22023 cannot subtract inet values of different sizes"),
        ("-", &["inet:255.255.255.255", "inet:0.0.0.0"], "4294967295"),
        ("-", &["cidr:10.0.0.0/8", "inet:9.255.255.255"], "1"),
        ("-", &["inet:10.0.0.1/8", "inet:10.0.0.1/24"], "0"),
    ];

    const FUNCS: &[(&str, &[&str], &str)] = &[
        ("host", &["inet:192.168.1.5/24"], "192.168.1.5"),
        ("host", &["inet:192.168.1.5"], "192.168.1.5"),
        ("host", &["inet:10.0.0.0/8"], "10.0.0.0"),
        ("host", &["inet:10.1.2.3/16"], "10.1.2.3"),
        ("host", &["inet:::1"], "::1"),
        ("host", &["inet:::/0"], "::"),
        ("host", &["inet:2001:db8::1/64"], "2001:db8::1"),
        ("host", &["inet:2001:db8::/32"], "2001:db8::"),
        ("host", &["cidr:10.0.0.0/8"], "10.0.0.0"),
        ("host", &["cidr:192.168.1.0/24"], "192.168.1.0"),
        ("host", &["cidr:2001:db8::/32"], "2001:db8::"),
        ("host", &["cidr:::/0"], "::"),
        ("text", &["inet:192.168.1.5/24"], "192.168.1.5/24"),
        ("text", &["inet:192.168.1.5"], "192.168.1.5/32"),
        ("text", &["inet:10.0.0.0/8"], "10.0.0.0/8"),
        ("text", &["inet:10.1.2.3/16"], "10.1.2.3/16"),
        ("text", &["inet:::1"], "::1/128"),
        ("text", &["inet:::/0"], "::/0"),
        ("text", &["inet:2001:db8::1/64"], "2001:db8::1/64"),
        ("text", &["inet:2001:db8::/32"], "2001:db8::/32"),
        ("text", &["cidr:10.0.0.0/8"], "10.0.0.0/8"),
        ("text", &["cidr:192.168.1.0/24"], "192.168.1.0/24"),
        ("text", &["cidr:2001:db8::/32"], "2001:db8::/32"),
        ("text", &["cidr:::/0"], "::/0"),
        ("abbrev", &["inet:192.168.1.5/24"], "192.168.1.5/24"),
        ("abbrev", &["inet:192.168.1.5"], "192.168.1.5"),
        ("abbrev", &["inet:10.0.0.0/8"], "10.0.0.0/8"),
        ("abbrev", &["inet:10.1.2.3/16"], "10.1.2.3/16"),
        ("abbrev", &["inet:::1"], "::1"),
        ("abbrev", &["inet:::/0"], "::/0"),
        ("abbrev", &["inet:2001:db8::1/64"], "2001:db8::1/64"),
        ("abbrev", &["inet:2001:db8::/32"], "2001:db8::/32"),
        ("abbrev", &["cidr:10.0.0.0/8"], "10/8"),
        ("abbrev", &["cidr:192.168.1.0/24"], "192.168.1/24"),
        ("abbrev", &["cidr:2001:db8::/32"], "2001:db8/32"),
        ("abbrev", &["cidr:::/0"], "::/0"),
        ("broadcast", &["inet:192.168.1.5/24"], "192.168.1.255/24"),
        ("broadcast", &["inet:192.168.1.5"], "192.168.1.5"),
        ("broadcast", &["inet:10.0.0.0/8"], "10.255.255.255/8"),
        ("broadcast", &["inet:10.1.2.3/16"], "10.1.255.255/16"),
        ("broadcast", &["inet:::1"], "::1"),
        ("broadcast", &["inet:::/0"], "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/0"),
        ("broadcast", &["inet:2001:db8::1/64"], "2001:db8::ffff:ffff:ffff:ffff/64"),
        ("broadcast", &["inet:2001:db8::/32"], "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff/32"),
        ("broadcast", &["cidr:10.0.0.0/8"], "10.255.255.255/8"),
        ("broadcast", &["cidr:192.168.1.0/24"], "192.168.1.255/24"),
        ("broadcast", &["cidr:2001:db8::/32"], "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff/32"),
        ("broadcast", &["cidr:::/0"], "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/0"),
        ("netmask", &["inet:192.168.1.5/24"], "255.255.255.0"),
        ("netmask", &["inet:192.168.1.5"], "255.255.255.255"),
        ("netmask", &["inet:10.0.0.0/8"], "255.0.0.0"),
        ("netmask", &["inet:10.1.2.3/16"], "255.255.0.0"),
        ("netmask", &["inet:::1"], "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
        ("netmask", &["inet:::/0"], "::"),
        ("netmask", &["inet:2001:db8::1/64"], "ffff:ffff:ffff:ffff::"),
        ("netmask", &["inet:2001:db8::/32"], "ffff:ffff::"),
        ("netmask", &["cidr:10.0.0.0/8"], "255.0.0.0"),
        ("netmask", &["cidr:192.168.1.0/24"], "255.255.255.0"),
        ("netmask", &["cidr:2001:db8::/32"], "ffff:ffff::"),
        ("netmask", &["cidr:::/0"], "::"),
        ("hostmask", &["inet:192.168.1.5/24"], "0.0.0.255"),
        ("hostmask", &["inet:192.168.1.5"], "0.0.0.0"),
        ("hostmask", &["inet:10.0.0.0/8"], "0.255.255.255"),
        ("hostmask", &["inet:10.1.2.3/16"], "0.0.255.255"),
        ("hostmask", &["inet:::1"], "::"),
        ("hostmask", &["inet:::/0"], "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
        ("hostmask", &["inet:2001:db8::1/64"], "::ffff:ffff:ffff:ffff"),
        ("hostmask", &["inet:2001:db8::/32"], "::ffff:ffff:ffff:ffff:ffff:ffff"),
        ("hostmask", &["cidr:10.0.0.0/8"], "0.255.255.255"),
        ("hostmask", &["cidr:192.168.1.0/24"], "0.0.0.255"),
        ("hostmask", &["cidr:2001:db8::/32"], "::ffff:ffff:ffff:ffff:ffff:ffff"),
        ("hostmask", &["cidr:::/0"], "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
        ("network", &["inet:192.168.1.5/24"], "192.168.1.0/24"),
        ("network", &["inet:192.168.1.5"], "192.168.1.5/32"),
        ("network", &["inet:10.0.0.0/8"], "10.0.0.0/8"),
        ("network", &["inet:10.1.2.3/16"], "10.1.0.0/16"),
        ("network", &["inet:::1"], "::1/128"),
        ("network", &["inet:::/0"], "::/0"),
        ("network", &["inet:2001:db8::1/64"], "2001:db8::/64"),
        ("network", &["inet:2001:db8::/32"], "2001:db8::/32"),
        ("network", &["cidr:10.0.0.0/8"], "10.0.0.0/8"),
        ("network", &["cidr:192.168.1.0/24"], "192.168.1.0/24"),
        ("network", &["cidr:2001:db8::/32"], "2001:db8::/32"),
        ("network", &["cidr:::/0"], "::/0"),
        ("masklen", &["inet:192.168.1.5/24"], "24"),
        ("masklen", &["inet:192.168.1.5"], "32"),
        ("masklen", &["inet:10.0.0.0/8"], "8"),
        ("masklen", &["inet:10.1.2.3/16"], "16"),
        ("masklen", &["inet:::1"], "128"),
        ("masklen", &["inet:::/0"], "0"),
        ("masklen", &["inet:2001:db8::1/64"], "64"),
        ("masklen", &["inet:2001:db8::/32"], "32"),
        ("masklen", &["cidr:10.0.0.0/8"], "8"),
        ("masklen", &["cidr:192.168.1.0/24"], "24"),
        ("masklen", &["cidr:2001:db8::/32"], "32"),
        ("masklen", &["cidr:::/0"], "0"),
        ("family", &["inet:192.168.1.5/24"], "4"),
        ("family", &["inet:192.168.1.5"], "4"),
        ("family", &["inet:10.0.0.0/8"], "4"),
        ("family", &["inet:10.1.2.3/16"], "4"),
        ("family", &["inet:::1"], "6"),
        ("family", &["inet:::/0"], "6"),
        ("family", &["inet:2001:db8::1/64"], "6"),
        ("family", &["inet:2001:db8::/32"], "6"),
        ("family", &["cidr:10.0.0.0/8"], "4"),
        ("family", &["cidr:192.168.1.0/24"], "4"),
        ("family", &["cidr:2001:db8::/32"], "6"),
        ("family", &["cidr:::/0"], "6"),
        ("abbrev", &["cidr:0.0.0.0/0"], "0/0"),
        ("abbrev", &["cidr:172.16.0.0/12"], "172.16/12"),
        ("abbrev", &["cidr:10.1.0.0/16"], "10.1/16"),
        ("abbrev", &["cidr:192.168.1.128/25"], "192.168.1.128/25"),
        ("abbrev", &["cidr:1.2.3.4/32"], "1.2.3.4/32"),
        ("abbrev", &["cidr:fe80::/10"], "fe80::/10"),
        ("abbrev", &["cidr:2001:db8:0:1::/64"], "2001:db8::1/64"),
        ("abbrev", &["cidr:::ffff:0:0/96"], "::ffff/96"),
        ("abbrev", &["cidr:ff00::/8"], "ff00::/8"),
        ("abbrev", &["cidr:0:0:1:0:0:2:0:0/112"], "0:0:1::0/112"),
        ("abbrev", &["cidr:0:0:1:0:0:2:0:5/128"], "0:0:1::0:5/128"),
        ("abbrev", &["cidr:::8000/113"], ":.128/113"),
        ("abbrev", &["cidr:::ffff/128"], ":.255.255/128"),
        ("abbrev", &["cidr:::100/120"], ":.1/120"),
        ("abbrev", &["cidr:::101/128"], "::101/128"),
        ("abbrev", &["cidr:::ffff:1.0.0.0/104"], "::ffff:1.0/104"),
        ("abbrev", &["cidr:::1.0.0.0/104"], "::1.0/104"),
        ("abbrev", &["cidr:1:0:2:0:0:3:0:0/113"], "1:0:2::3:0:0/113"),
        ("abbrev", &["cidr:::2/127"], "::2/127"),
        ("set_masklen", &["inet:192.168.1.5/24", "int:-2"], "22023 invalid mask length: -2"),
        ("set_masklen", &["inet:192.168.1.5/24", "int:-1"], "192.168.1.5"),
        ("set_masklen", &["inet:192.168.1.5/24", "int:0"], "192.168.1.5/0"),
        ("set_masklen", &["inet:192.168.1.5/24", "int:24"], "192.168.1.5/24"),
        ("set_masklen", &["inet:192.168.1.5/24", "int:32"], "192.168.1.5"),
        ("set_masklen", &["inet:192.168.1.5/24", "int:33"], "22023 invalid mask length: 33"),
        ("set_masklen", &["inet:192.168.1.5/24", "int:64"], "22023 invalid mask length: 64"),
        ("set_masklen", &["inet:192.168.1.5/24", "int:129"], "22023 invalid mask length: 129"),
        ("set_masklen", &["cidr:192.168.1.0/24", "int:-2"], "22023 invalid mask length: -2"),
        ("set_masklen", &["cidr:192.168.1.0/24", "int:-1"], "192.168.1.0/32"),
        ("set_masklen", &["cidr:192.168.1.0/24", "int:0"], "0.0.0.0/0"),
        ("set_masklen", &["cidr:192.168.1.0/24", "int:24"], "192.168.1.0/24"),
        ("set_masklen", &["cidr:192.168.1.0/24", "int:32"], "192.168.1.0/32"),
        ("set_masklen", &["cidr:192.168.1.0/24", "int:33"], "22023 invalid mask length: 33"),
        ("set_masklen", &["cidr:192.168.1.0/24", "int:64"], "22023 invalid mask length: 64"),
        ("set_masklen", &["cidr:192.168.1.0/24", "int:129"], "22023 invalid mask length: 129"),
        ("set_masklen", &["inet:2001:db8::1/64", "int:-2"], "22023 invalid mask length: -2"),
        ("set_masklen", &["inet:2001:db8::1/64", "int:-1"], "2001:db8::1"),
        ("set_masklen", &["inet:2001:db8::1/64", "int:0"], "2001:db8::1/0"),
        ("set_masklen", &["inet:2001:db8::1/64", "int:24"], "2001:db8::1/24"),
        ("set_masklen", &["inet:2001:db8::1/64", "int:32"], "2001:db8::1/32"),
        ("set_masklen", &["inet:2001:db8::1/64", "int:33"], "2001:db8::1/33"),
        ("set_masklen", &["inet:2001:db8::1/64", "int:64"], "2001:db8::1/64"),
        ("set_masklen", &["inet:2001:db8::1/64", "int:129"], "22023 invalid mask length: 129"),
        ("set_masklen", &["inet:1.2.3.4", "int:-2"], "22023 invalid mask length: -2"),
        ("set_masklen", &["inet:1.2.3.4", "int:-1"], "1.2.3.4"),
        ("set_masklen", &["inet:1.2.3.4", "int:0"], "1.2.3.4/0"),
        ("set_masklen", &["inet:1.2.3.4", "int:24"], "1.2.3.4/24"),
        ("set_masklen", &["inet:1.2.3.4", "int:32"], "1.2.3.4"),
        ("set_masklen", &["inet:1.2.3.4", "int:33"], "22023 invalid mask length: 33"),
        ("set_masklen", &["inet:1.2.3.4", "int:64"], "22023 invalid mask length: 64"),
        ("set_masklen", &["inet:1.2.3.4", "int:129"], "22023 invalid mask length: 129"),
        ("inet_merge", &["inet:192.168.1.5/24", "inet:192.168.2.5/24"], "192.168.0.0/22"),
        ("inet_same_family", &["inet:192.168.1.5/24", "inet:192.168.2.5/24"], "t"),
        ("inet_merge", &["inet:192.168.1.5/24", "inet:192.168.1.5/24"], "192.168.1.0/24"),
        ("inet_same_family", &["inet:192.168.1.5/24", "inet:192.168.1.5/24"], "t"),
        ("inet_merge", &["inet:10.0.0.1", "inet:10.0.0.2"], "10.0.0.0/30"),
        ("inet_same_family", &["inet:10.0.0.1", "inet:10.0.0.2"], "t"),
        ("inet_merge", &["inet:1.2.3.4", "inet:::1"], "22023 cannot merge addresses from different families"),
        ("inet_same_family", &["inet:1.2.3.4", "inet:::1"], "f"),
        ("inet_merge", &["cidr:10.0.0.0/8", "cidr:11.0.0.0/8"], "10.0.0.0/7"),
        ("inet_same_family", &["cidr:10.0.0.0/8", "cidr:11.0.0.0/8"], "t"),
        ("inet_merge", &["inet:2001:db8::1", "inet:2001:db8::8000:0:1"], "2001:db8::/80"),
        ("inet_same_family", &["inet:2001:db8::1", "inet:2001:db8::8000:0:1"], "t"),
        ("inet_merge", &["inet:0.0.0.0/0", "inet:255.255.255.255"], "0.0.0.0/0"),
        ("inet_same_family", &["inet:0.0.0.0/0", "inet:255.255.255.255"], "t"),
        ("inet_merge", &["inet:192.168.1.5/16", "inet:192.168.1.5/24"], "192.168.0.0/16"),
        ("inet_same_family", &["inet:192.168.1.5/16", "inet:192.168.1.5/24"], "t"),
        ("to_cidr", &["inet:192.168.1.5/24"], "192.168.1.0/24"),
        ("to_cidr", &["inet:192.168.1.5"], "192.168.1.5/32"),
        ("to_cidr", &["inet:2001:db8::1/64"], "2001:db8::/64"),
        ("to_cidr", &["inet:::1"], "::1/128"),
        ("to_cidr", &["inet:10.0.0.0/8"], "10.0.0.0/8"),
        ("to_inet", &["cidr:10.0.0.0/8"], "10.0.0.0/8"),
        ("to_inet", &["cidr:192.168.1.0/24"], "192.168.1.0/24"),
        ("to_inet", &["cidr:1.2.3.4/32"], "1.2.3.4"),
        ("to_inet", &["cidr:::1/128"], "::1"),
        ("trunc", &["macaddr:08:00:2b:01:02:03"], "08:00:2b:00:00:00"),
        ("to_macaddr8", &["macaddr:08:00:2b:01:02:03"], "08:00:2b:ff:fe:01:02:03"),
        ("trunc", &["macaddr:ff:ff:ff:ff:ff:ff"], "ff:ff:ff:00:00:00"),
        ("to_macaddr8", &["macaddr:ff:ff:ff:ff:ff:ff"], "ff:ff:ff:ff:fe:ff:ff:ff"),
        ("trunc", &["macaddr:00:00:00:ff:fe:00"], "00:00:00:00:00:00"),
        ("to_macaddr8", &["macaddr:00:00:00:ff:fe:00"], "00:00:00:ff:fe:ff:fe:00"),
        ("trunc", &["macaddr8:08:00:2b:01:02:03:04:05"], "08:00:2b:00:00:00:00:00"),
        ("to_macaddr", &["macaddr8:08:00:2b:01:02:03:04:05"], "22003 macaddr8 data out of range to convert to macaddr"),
        ("macaddr8_set7bit", &["macaddr8:08:00:2b:01:02:03:04:05"], "0a:00:2b:01:02:03:04:05"),
        ("trunc", &["macaddr8:ff:ff:ff:ff:ff:ff:ff:ff"], "ff:ff:ff:00:00:00:00:00"),
        ("to_macaddr", &["macaddr8:ff:ff:ff:ff:ff:ff:ff:ff"], "22003 macaddr8 data out of range to convert to macaddr"),
        ("macaddr8_set7bit", &["macaddr8:ff:ff:ff:ff:ff:ff:ff:ff"], "ff:ff:ff:ff:ff:ff:ff:ff"),
        ("trunc", &["macaddr8:08:00:2b:ff:fe:01:02:03"], "08:00:2b:00:00:00:00:00"),
        ("to_macaddr", &["macaddr8:08:00:2b:ff:fe:01:02:03"], "08:00:2b:01:02:03"),
        ("macaddr8_set7bit", &["macaddr8:08:00:2b:ff:fe:01:02:03"], "0a:00:2b:ff:fe:01:02:03"),
        ("macaddr8_set7bit", &["macaddr8:0a:00:2b:ff:fe:01:02:03"], "0a:00:2b:ff:fe:01:02:03"),
    ];

    // The same, for calls no overload takes: rows are `op`, the argument types, what Postgres said. A
    // type name stands for its `sample`, and `&` and friends are asked of Postgres with the operands
    // written as typed literals, `inet '10.1.2.3/8' & macaddr '08:00:2b:01:02:03'`.

    const OPERATOR_TYPES: &[(&str, &[&str], &str)] = &[
        ("&", &["inet", "inet"], "10.1.2.3/8"),
        ("&", &["inet", "cidr"], "10.0.0.0/8"),
        ("&", &["inet", "macaddr"], "42883 operator does not exist: inet & macaddr"),
        ("&", &["inet", "macaddr8"], "42883 operator does not exist: inet & macaddr8"),
        ("&", &["inet", "smallint"], "42883 operator does not exist: inet & smallint"),
        ("&", &["inet", "int"], "42883 operator does not exist: inet & integer"),
        ("&", &["inet", "bigint"], "42883 operator does not exist: inet & bigint"),
        ("&", &["cidr", "inet"], "10.0.0.0/8"),
        ("&", &["cidr", "cidr"], "10.0.0.0/8"),
        ("&", &["cidr", "macaddr"], "42883 operator does not exist: cidr & macaddr"),
        ("&", &["cidr", "macaddr8"], "42883 operator does not exist: cidr & macaddr8"),
        ("&", &["cidr", "smallint"], "42883 operator does not exist: cidr & smallint"),
        ("&", &["cidr", "int"], "42883 operator does not exist: cidr & integer"),
        ("&", &["cidr", "bigint"], "42883 operator does not exist: cidr & bigint"),
        ("&", &["macaddr", "inet"], "42883 operator does not exist: macaddr & inet"),
        ("&", &["macaddr", "cidr"], "42883 operator does not exist: macaddr & cidr"),
        ("&", &["macaddr", "macaddr"], "08:00:2b:01:02:03"),
        ("&", &["macaddr", "macaddr8"], "42725 operator is not unique: macaddr & macaddr8"),
        ("&", &["macaddr", "smallint"], "42883 operator does not exist: macaddr & smallint"),
        ("&", &["macaddr", "int"], "42883 operator does not exist: macaddr & integer"),
        ("&", &["macaddr", "bigint"], "42883 operator does not exist: macaddr & bigint"),
        ("&", &["macaddr8", "inet"], "42883 operator does not exist: macaddr8 & inet"),
        ("&", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 & cidr"),
        ("&", &["macaddr8", "macaddr"], "42725 operator is not unique: macaddr8 & macaddr"),
        ("&", &["macaddr8", "macaddr8"], "08:00:2b:ff:fe:01:02:03"),
        ("&", &["macaddr8", "smallint"], "42883 operator does not exist: macaddr8 & smallint"),
        ("&", &["macaddr8", "int"], "42883 operator does not exist: macaddr8 & integer"),
        ("&", &["macaddr8", "bigint"], "42883 operator does not exist: macaddr8 & bigint"),
        ("&", &["smallint", "inet"], "42883 operator does not exist: smallint & inet"),
        ("&", &["smallint", "cidr"], "42883 operator does not exist: smallint & cidr"),
        ("&", &["smallint", "macaddr"], "42883 operator does not exist: smallint & macaddr"),
        ("&", &["smallint", "macaddr8"], "42883 operator does not exist: smallint & macaddr8"),
        ("&", &["int", "inet"], "42883 operator does not exist: integer & inet"),
        ("&", &["int", "cidr"], "42883 operator does not exist: integer & cidr"),
        ("&", &["int", "macaddr"], "42883 operator does not exist: integer & macaddr"),
        ("&", &["int", "macaddr8"], "42883 operator does not exist: integer & macaddr8"),
        ("&", &["bigint", "inet"], "42883 operator does not exist: bigint & inet"),
        ("&", &["bigint", "cidr"], "42883 operator does not exist: bigint & cidr"),
        ("&", &["bigint", "macaddr"], "42883 operator does not exist: bigint & macaddr"),
        ("&", &["bigint", "macaddr8"], "42883 operator does not exist: bigint & macaddr8"),
        ("+", &["inet", "inet"], "42883 operator does not exist: inet + inet"),
        ("+", &["inet", "cidr"], "42883 operator does not exist: inet + cidr"),
        ("+", &["inet", "macaddr"], "42883 operator does not exist: inet + macaddr"),
        ("+", &["inet", "macaddr8"], "42883 operator does not exist: inet + macaddr8"),
        ("+", &["inet", "smallint"], "10.1.2.8/8"),
        ("+", &["inet", "int"], "10.1.2.8/8"),
        ("+", &["inet", "bigint"], "10.1.2.8/8"),
        ("+", &["cidr", "inet"], "42883 operator does not exist: cidr + inet"),
        ("+", &["cidr", "cidr"], "42883 operator does not exist: cidr + cidr"),
        ("+", &["cidr", "macaddr"], "42883 operator does not exist: cidr + macaddr"),
        ("+", &["cidr", "macaddr8"], "42883 operator does not exist: cidr + macaddr8"),
        ("+", &["cidr", "smallint"], "10.0.0.5/8"),
        ("+", &["cidr", "int"], "10.0.0.5/8"),
        ("+", &["cidr", "bigint"], "10.0.0.5/8"),
        ("+", &["macaddr", "inet"], "42883 operator does not exist: macaddr + inet"),
        ("+", &["macaddr", "cidr"], "42883 operator does not exist: macaddr + cidr"),
        ("+", &["macaddr", "macaddr"], "42883 operator does not exist: macaddr + macaddr"),
        ("+", &["macaddr", "macaddr8"], "42883 operator does not exist: macaddr + macaddr8"),
        ("+", &["macaddr", "smallint"], "42883 operator does not exist: macaddr + smallint"),
        ("+", &["macaddr", "int"], "42883 operator does not exist: macaddr + integer"),
        ("+", &["macaddr", "bigint"], "42883 operator does not exist: macaddr + bigint"),
        ("+", &["macaddr8", "inet"], "42883 operator does not exist: macaddr8 + inet"),
        ("+", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 + cidr"),
        ("+", &["macaddr8", "macaddr"], "42883 operator does not exist: macaddr8 + macaddr"),
        ("+", &["macaddr8", "macaddr8"], "42883 operator does not exist: macaddr8 + macaddr8"),
        ("+", &["macaddr8", "smallint"], "42883 operator does not exist: macaddr8 + smallint"),
        ("+", &["macaddr8", "int"], "42883 operator does not exist: macaddr8 + integer"),
        ("+", &["macaddr8", "bigint"], "42883 operator does not exist: macaddr8 + bigint"),
        ("+", &["smallint", "inet"], "10.1.2.8/8"),
        ("+", &["smallint", "cidr"], "10.0.0.5/8"),
        ("+", &["smallint", "macaddr"], "42883 operator does not exist: smallint + macaddr"),
        ("+", &["smallint", "macaddr8"], "42883 operator does not exist: smallint + macaddr8"),
        ("+", &["int", "inet"], "10.1.2.8/8"),
        ("+", &["int", "cidr"], "10.0.0.5/8"),
        ("+", &["int", "macaddr"], "42883 operator does not exist: integer + macaddr"),
        ("+", &["int", "macaddr8"], "42883 operator does not exist: integer + macaddr8"),
        ("+", &["bigint", "inet"], "10.1.2.8/8"),
        ("+", &["bigint", "cidr"], "10.0.0.5/8"),
        ("+", &["bigint", "macaddr"], "42883 operator does not exist: bigint + macaddr"),
        ("+", &["bigint", "macaddr8"], "42883 operator does not exist: bigint + macaddr8"),
        ("-", &["inet", "inet"], "0"),
        ("-", &["inet", "cidr"], "66051"),
        ("-", &["inet", "macaddr"], "42883 operator does not exist: inet - macaddr"),
        ("-", &["inet", "macaddr8"], "42883 operator does not exist: inet - macaddr8"),
        ("-", &["inet", "smallint"], "10.1.1.254/8"),
        ("-", &["inet", "int"], "10.1.1.254/8"),
        ("-", &["inet", "bigint"], "10.1.1.254/8"),
        ("-", &["cidr", "inet"], "-66051"),
        ("-", &["cidr", "cidr"], "0"),
        ("-", &["cidr", "macaddr"], "42883 operator does not exist: cidr - macaddr"),
        ("-", &["cidr", "macaddr8"], "42883 operator does not exist: cidr - macaddr8"),
        ("-", &["cidr", "smallint"], "9.255.255.251/8"),
        ("-", &["cidr", "int"], "9.255.255.251/8"),
        ("-", &["cidr", "bigint"], "9.255.255.251/8"),
        ("-", &["macaddr", "inet"], "42883 operator does not exist: macaddr - inet"),
        ("-", &["macaddr", "cidr"], "42883 operator does not exist: macaddr - cidr"),
        ("-", &["macaddr", "macaddr"], "42883 operator does not exist: macaddr - macaddr"),
        ("-", &["macaddr", "macaddr8"], "42883 operator does not exist: macaddr - macaddr8"),
        ("-", &["macaddr", "smallint"], "42883 operator does not exist: macaddr - smallint"),
        ("-", &["macaddr", "int"], "42883 operator does not exist: macaddr - integer"),
        ("-", &["macaddr", "bigint"], "42883 operator does not exist: macaddr - bigint"),
        ("-", &["macaddr8", "inet"], "42883 operator does not exist: macaddr8 - inet"),
        ("-", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 - cidr"),
        ("-", &["macaddr8", "macaddr"], "42883 operator does not exist: macaddr8 - macaddr"),
        ("-", &["macaddr8", "macaddr8"], "42883 operator does not exist: macaddr8 - macaddr8"),
        ("-", &["macaddr8", "smallint"], "42883 operator does not exist: macaddr8 - smallint"),
        ("-", &["macaddr8", "int"], "42883 operator does not exist: macaddr8 - integer"),
        ("-", &["macaddr8", "bigint"], "42883 operator does not exist: macaddr8 - bigint"),
        ("-", &["smallint", "inet"], "42883 operator does not exist: smallint - inet"),
        ("-", &["smallint", "cidr"], "42883 operator does not exist: smallint - cidr"),
        ("-", &["smallint", "macaddr"], "42883 operator does not exist: smallint - macaddr"),
        ("-", &["smallint", "macaddr8"], "42883 operator does not exist: smallint - macaddr8"),
        ("-", &["int", "inet"], "42883 operator does not exist: integer - inet"),
        ("-", &["int", "cidr"], "42883 operator does not exist: integer - cidr"),
        ("-", &["int", "macaddr"], "42883 operator does not exist: integer - macaddr"),
        ("-", &["int", "macaddr8"], "42883 operator does not exist: integer - macaddr8"),
        ("-", &["bigint", "inet"], "42883 operator does not exist: bigint - inet"),
        ("-", &["bigint", "cidr"], "42883 operator does not exist: bigint - cidr"),
        ("-", &["bigint", "macaddr"], "42883 operator does not exist: bigint - macaddr"),
        ("-", &["bigint", "macaddr8"], "42883 operator does not exist: bigint - macaddr8"),
        ("<<", &["inet", "inet"], "f"),
        ("<<", &["inet", "cidr"], "f"),
        ("<<", &["inet", "macaddr"], "42883 operator does not exist: inet << macaddr"),
        ("<<", &["inet", "macaddr8"], "42883 operator does not exist: inet << macaddr8"),
        ("<<", &["inet", "smallint"], "42883 operator does not exist: inet << smallint"),
        ("<<", &["inet", "int"], "42883 operator does not exist: inet << integer"),
        ("<<", &["inet", "bigint"], "42883 operator does not exist: inet << bigint"),
        ("<<", &["cidr", "inet"], "f"),
        ("<<", &["cidr", "cidr"], "f"),
        ("<<", &["cidr", "macaddr"], "42883 operator does not exist: cidr << macaddr"),
        ("<<", &["cidr", "macaddr8"], "42883 operator does not exist: cidr << macaddr8"),
        ("<<", &["cidr", "smallint"], "42883 operator does not exist: cidr << smallint"),
        ("<<", &["cidr", "int"], "42883 operator does not exist: cidr << integer"),
        ("<<", &["cidr", "bigint"], "42883 operator does not exist: cidr << bigint"),
        ("<<", &["macaddr", "inet"], "42883 operator does not exist: macaddr << inet"),
        ("<<", &["macaddr", "cidr"], "42883 operator does not exist: macaddr << cidr"),
        ("<<", &["macaddr", "macaddr"], "42883 operator does not exist: macaddr << macaddr"),
        ("<<", &["macaddr", "macaddr8"], "42883 operator does not exist: macaddr << macaddr8"),
        ("<<", &["macaddr", "smallint"], "42883 operator does not exist: macaddr << smallint"),
        ("<<", &["macaddr", "int"], "42883 operator does not exist: macaddr << integer"),
        ("<<", &["macaddr", "bigint"], "42883 operator does not exist: macaddr << bigint"),
        ("<<", &["macaddr8", "inet"], "42883 operator does not exist: macaddr8 << inet"),
        ("<<", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 << cidr"),
        ("<<", &["macaddr8", "macaddr"], "42883 operator does not exist: macaddr8 << macaddr"),
        ("<<", &["macaddr8", "macaddr8"], "42883 operator does not exist: macaddr8 << macaddr8"),
        ("<<", &["macaddr8", "smallint"], "42883 operator does not exist: macaddr8 << smallint"),
        ("<<", &["macaddr8", "int"], "42883 operator does not exist: macaddr8 << integer"),
        ("<<", &["macaddr8", "bigint"], "42883 operator does not exist: macaddr8 << bigint"),
        ("<<", &["smallint", "inet"], "42883 operator does not exist: smallint << inet"),
        ("<<", &["smallint", "cidr"], "42883 operator does not exist: smallint << cidr"),
        ("<<", &["smallint", "macaddr"], "42883 operator does not exist: smallint << macaddr"),
        ("<<", &["smallint", "macaddr8"], "42883 operator does not exist: smallint << macaddr8"),
        ("<<", &["int", "inet"], "42883 operator does not exist: integer << inet"),
        ("<<", &["int", "cidr"], "42883 operator does not exist: integer << cidr"),
        ("<<", &["int", "macaddr"], "42883 operator does not exist: integer << macaddr"),
        ("<<", &["int", "macaddr8"], "42883 operator does not exist: integer << macaddr8"),
        ("<<", &["bigint", "inet"], "42883 operator does not exist: bigint << inet"),
        ("<<", &["bigint", "cidr"], "42883 operator does not exist: bigint << cidr"),
        ("<<", &["bigint", "macaddr"], "42883 operator does not exist: bigint << macaddr"),
        ("<<", &["bigint", "macaddr8"], "42883 operator does not exist: bigint << macaddr8"),
        ("|", &["inet", "macaddr"], "42883 operator does not exist: inet | macaddr"),
        ("|", &["macaddr", "inet"], "42883 operator does not exist: macaddr | inet"),
        ("|", &["macaddr", "macaddr8"], "42725 operator is not unique: macaddr | macaddr8"),
        ("|", &["macaddr8", "macaddr"], "42725 operator is not unique: macaddr8 | macaddr"),
        ("|", &["cidr", "macaddr8"], "42883 operator does not exist: cidr | macaddr8"),
        ("|", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 | cidr"),
        ("|", &["inet", "int"], "42883 operator does not exist: inet | integer"),
        ("=", &["inet", "macaddr"], "42883 operator does not exist: inet = macaddr"),
        ("=", &["macaddr", "inet"], "42883 operator does not exist: macaddr = inet"),
        ("=", &["macaddr", "macaddr8"], "42725 operator is not unique: macaddr = macaddr8"),
        ("=", &["macaddr8", "macaddr"], "42725 operator is not unique: macaddr8 = macaddr"),
        ("=", &["cidr", "macaddr8"], "42883 operator does not exist: cidr = macaddr8"),
        ("=", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 = cidr"),
        ("=", &["inet", "int"], "42883 operator does not exist: inet = integer"),
        ("<>", &["inet", "macaddr"], "42883 operator does not exist: inet <> macaddr"),
        ("<>", &["macaddr", "inet"], "42883 operator does not exist: macaddr <> inet"),
        ("<>", &["macaddr", "macaddr8"], "42725 operator is not unique: macaddr <> macaddr8"),
        ("<>", &["macaddr8", "macaddr"], "42725 operator is not unique: macaddr8 <> macaddr"),
        ("<>", &["cidr", "macaddr8"], "42883 operator does not exist: cidr <> macaddr8"),
        ("<>", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 <> cidr"),
        ("<>", &["inet", "int"], "42883 operator does not exist: inet <> integer"),
        ("<", &["inet", "macaddr"], "42883 operator does not exist: inet < macaddr"),
        ("<", &["macaddr", "inet"], "42883 operator does not exist: macaddr < inet"),
        ("<", &["macaddr", "macaddr8"], "42725 operator is not unique: macaddr < macaddr8"),
        ("<", &["macaddr8", "macaddr"], "42725 operator is not unique: macaddr8 < macaddr"),
        ("<", &["cidr", "macaddr8"], "42883 operator does not exist: cidr < macaddr8"),
        ("<", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 < cidr"),
        ("<", &["inet", "int"], "42883 operator does not exist: inet < integer"),
        ("<=", &["inet", "macaddr"], "42883 operator does not exist: inet <= macaddr"),
        ("<=", &["macaddr", "inet"], "42883 operator does not exist: macaddr <= inet"),
        ("<=", &["macaddr", "macaddr8"], "42725 operator is not unique: macaddr <= macaddr8"),
        ("<=", &["macaddr8", "macaddr"], "42725 operator is not unique: macaddr8 <= macaddr"),
        ("<=", &["cidr", "macaddr8"], "42883 operator does not exist: cidr <= macaddr8"),
        ("<=", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 <= cidr"),
        ("<=", &["inet", "int"], "42883 operator does not exist: inet <= integer"),
        (">", &["inet", "macaddr"], "42883 operator does not exist: inet > macaddr"),
        (">", &["macaddr", "inet"], "42883 operator does not exist: macaddr > inet"),
        (">", &["macaddr", "macaddr8"], "42725 operator is not unique: macaddr > macaddr8"),
        (">", &["macaddr8", "macaddr"], "42725 operator is not unique: macaddr8 > macaddr"),
        (">", &["cidr", "macaddr8"], "42883 operator does not exist: cidr > macaddr8"),
        (">", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 > cidr"),
        (">", &["inet", "int"], "42883 operator does not exist: inet > integer"),
        (">=", &["inet", "macaddr"], "42883 operator does not exist: inet >= macaddr"),
        (">=", &["macaddr", "inet"], "42883 operator does not exist: macaddr >= inet"),
        (">=", &["macaddr", "macaddr8"], "42725 operator is not unique: macaddr >= macaddr8"),
        (">=", &["macaddr8", "macaddr"], "42725 operator is not unique: macaddr8 >= macaddr"),
        (">=", &["cidr", "macaddr8"], "42883 operator does not exist: cidr >= macaddr8"),
        (">=", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 >= cidr"),
        (">=", &["inet", "int"], "42883 operator does not exist: inet >= integer"),
        ("<<=", &["inet", "macaddr"], "42883 operator does not exist: inet <<= macaddr"),
        ("<<=", &["macaddr", "inet"], "42883 operator does not exist: macaddr <<= inet"),
        ("<<=", &["macaddr", "macaddr8"], "42883 operator does not exist: macaddr <<= macaddr8"),
        ("<<=", &["macaddr8", "macaddr"], "42883 operator does not exist: macaddr8 <<= macaddr"),
        ("<<=", &["cidr", "macaddr8"], "42883 operator does not exist: cidr <<= macaddr8"),
        ("<<=", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 <<= cidr"),
        ("<<=", &["inet", "int"], "42883 operator does not exist: inet <<= integer"),
        (">>", &["inet", "macaddr"], "42883 operator does not exist: inet >> macaddr"),
        (">>", &["macaddr", "inet"], "42883 operator does not exist: macaddr >> inet"),
        (">>", &["macaddr", "macaddr8"], "42883 operator does not exist: macaddr >> macaddr8"),
        (">>", &["macaddr8", "macaddr"], "42883 operator does not exist: macaddr8 >> macaddr"),
        (">>", &["cidr", "macaddr8"], "42883 operator does not exist: cidr >> macaddr8"),
        (">>", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 >> cidr"),
        (">>", &["inet", "int"], "42883 operator does not exist: inet >> integer"),
        (">>=", &["inet", "macaddr"], "42883 operator does not exist: inet >>= macaddr"),
        (">>=", &["macaddr", "inet"], "42883 operator does not exist: macaddr >>= inet"),
        (">>=", &["macaddr", "macaddr8"], "42883 operator does not exist: macaddr >>= macaddr8"),
        (">>=", &["macaddr8", "macaddr"], "42883 operator does not exist: macaddr8 >>= macaddr"),
        (">>=", &["cidr", "macaddr8"], "42883 operator does not exist: cidr >>= macaddr8"),
        (">>=", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 >>= cidr"),
        (">>=", &["inet", "int"], "42883 operator does not exist: inet >>= integer"),
        ("&&", &["inet", "macaddr"], "42883 operator does not exist: inet && macaddr"),
        ("&&", &["macaddr", "inet"], "42883 operator does not exist: macaddr && inet"),
        ("&&", &["macaddr", "macaddr8"], "42883 operator does not exist: macaddr && macaddr8"),
        ("&&", &["macaddr8", "macaddr"], "42883 operator does not exist: macaddr8 && macaddr"),
        ("&&", &["cidr", "macaddr8"], "42883 operator does not exist: cidr && macaddr8"),
        ("&&", &["macaddr8", "cidr"], "42883 operator does not exist: macaddr8 && cidr"),
        ("&&", &["inet", "int"], "42883 operator does not exist: inet && integer"),
    ];

    const FUNCTION_TYPES: &[(&str, &[&str], &str)] = &[
        ("host", &["inet"], "10.1.2.3"),
        ("host", &["cidr"], "10.0.0.0"),
        ("host", &["macaddr"], "42883 function host(macaddr) does not exist"),
        ("host", &["macaddr8"], "42883 function host(macaddr8) does not exist"),
        ("text", &["inet"], "10.1.2.3/8"),
        ("text", &["cidr"], "10.0.0.0/8"),
        ("text", &["macaddr"], "08:00:2b:01:02:03"),
        ("text", &["macaddr8"], "08:00:2b:ff:fe:01:02:03"),
        ("abbrev", &["inet"], "10.1.2.3/8"),
        ("abbrev", &["cidr"], "10/8"),
        ("abbrev", &["macaddr"], "42883 function abbrev(macaddr) does not exist"),
        ("abbrev", &["macaddr8"], "42883 function abbrev(macaddr8) does not exist"),
        ("broadcast", &["inet"], "10.255.255.255/8"),
        ("broadcast", &["cidr"], "10.255.255.255/8"),
        ("broadcast", &["macaddr"], "42883 function broadcast(macaddr) does not exist"),
        ("broadcast", &["macaddr8"], "42883 function broadcast(macaddr8) does not exist"),
        ("netmask", &["inet"], "255.0.0.0"),
        ("netmask", &["cidr"], "255.0.0.0"),
        ("netmask", &["macaddr"], "42883 function netmask(macaddr) does not exist"),
        ("netmask", &["macaddr8"], "42883 function netmask(macaddr8) does not exist"),
        ("hostmask", &["inet"], "0.255.255.255"),
        ("hostmask", &["cidr"], "0.255.255.255"),
        ("hostmask", &["macaddr"], "42883 function hostmask(macaddr) does not exist"),
        ("hostmask", &["macaddr8"], "42883 function hostmask(macaddr8) does not exist"),
        ("network", &["inet"], "10.0.0.0/8"),
        ("network", &["cidr"], "10.0.0.0/8"),
        ("network", &["macaddr"], "42883 function network(macaddr) does not exist"),
        ("network", &["macaddr8"], "42883 function network(macaddr8) does not exist"),
        ("masklen", &["inet"], "8"),
        ("masklen", &["cidr"], "8"),
        ("masklen", &["macaddr"], "42883 function masklen(macaddr) does not exist"),
        ("masklen", &["macaddr8"], "42883 function masklen(macaddr8) does not exist"),
        ("family", &["inet"], "4"),
        ("family", &["cidr"], "4"),
        ("family", &["macaddr"], "42883 function family(macaddr) does not exist"),
        ("family", &["macaddr8"], "42883 function family(macaddr8) does not exist"),
        ("trunc", &["inet"], "42883 function trunc(inet) does not exist"),
        ("trunc", &["cidr"], "42883 function trunc(cidr) does not exist"),
        ("trunc", &["macaddr"], "08:00:2b:00:00:00"),
        ("trunc", &["macaddr8"], "08:00:2b:00:00:00:00:00"),
        ("macaddr8_set7bit", &["inet"], "42883 function macaddr8_set7bit(inet) does not exist"),
        ("macaddr8_set7bit", &["cidr"], "42883 function macaddr8_set7bit(cidr) does not exist"),
        ("macaddr8_set7bit", &["macaddr"], "0a:00:2b:ff:fe:01:02:03"),
        ("macaddr8_set7bit", &["macaddr8"], "0a:00:2b:ff:fe:01:02:03"),
        ("masklen", &["int"], "42883 function masklen(integer) does not exist"),
        ("masklen", &["int"], "42883 function masklen(integer) does not exist"),
        ("host", &["int"], "42883 function host(integer) does not exist"),
        ("set_masklen", &["inet", "smallint"], "10.1.2.3/5"),
        ("set_masklen", &["inet", "int"], "10.1.2.3/5"),
        ("set_masklen", &["inet", "bigint"], "42883 function set_masklen(inet, bigint) does not exist"),
        ("set_masklen", &["cidr", "smallint"], "8.0.0.0/5"),
        ("set_masklen", &["cidr", "int"], "8.0.0.0/5"),
        ("set_masklen", &["cidr", "bigint"], "42883 function set_masklen(cidr, bigint) does not exist"),
        ("set_masklen", &["macaddr", "smallint"], "42883 function set_masklen(macaddr, smallint) does not exist"),
        ("set_masklen", &["macaddr", "int"], "42883 function set_masklen(macaddr, integer) does not exist"),
        ("set_masklen", &["macaddr", "bigint"], "42883 function set_masklen(macaddr, bigint) does not exist"),
        ("set_masklen", &["macaddr8", "smallint"], "42883 function set_masklen(macaddr8, smallint) does not exist"),
        ("set_masklen", &["macaddr8", "int"], "42883 function set_masklen(macaddr8, integer) does not exist"),
        ("set_masklen", &["macaddr8", "bigint"], "42883 function set_masklen(macaddr8, bigint) does not exist"),
        ("inet_same_family", &["inet", "inet"], "t"),
        ("inet_same_family", &["inet", "cidr"], "t"),
        ("inet_same_family", &["inet", "macaddr"], "42883 function inet_same_family(inet, macaddr) does not exist"),
        ("inet_same_family", &["inet", "macaddr8"], "42883 function inet_same_family(inet, macaddr8) does not exist"),
        ("inet_same_family", &["cidr", "inet"], "t"),
        ("inet_same_family", &["cidr", "cidr"], "t"),
        ("inet_same_family", &["cidr", "macaddr"], "42883 function inet_same_family(cidr, macaddr) does not exist"),
        ("inet_same_family", &["cidr", "macaddr8"], "42883 function inet_same_family(cidr, macaddr8) does not exist"),
        ("inet_same_family", &["macaddr", "inet"], "42883 function inet_same_family(macaddr, inet) does not exist"),
        ("inet_same_family", &["macaddr", "cidr"], "42883 function inet_same_family(macaddr, cidr) does not exist"),
        (
            "inet_same_family",
            &["macaddr", "macaddr"],
            "42883 function inet_same_family(macaddr, macaddr) does not exist",
        ),
        (
            "inet_same_family",
            &["macaddr", "macaddr8"],
            "42883 function inet_same_family(macaddr, macaddr8) does not exist",
        ),
        ("inet_same_family", &["macaddr8", "inet"], "42883 function inet_same_family(macaddr8, inet) does not exist"),
        ("inet_same_family", &["macaddr8", "cidr"], "42883 function inet_same_family(macaddr8, cidr) does not exist"),
        (
            "inet_same_family",
            &["macaddr8", "macaddr"],
            "42883 function inet_same_family(macaddr8, macaddr) does not exist",
        ),
        (
            "inet_same_family",
            &["macaddr8", "macaddr8"],
            "42883 function inet_same_family(macaddr8, macaddr8) does not exist",
        ),
        ("inet_merge", &["inet", "inet"], "10.0.0.0/8"),
        ("inet_merge", &["inet", "cidr"], "10.0.0.0/8"),
        ("inet_merge", &["inet", "macaddr"], "42883 function inet_merge(inet, macaddr) does not exist"),
        ("inet_merge", &["inet", "macaddr8"], "42883 function inet_merge(inet, macaddr8) does not exist"),
        ("inet_merge", &["cidr", "inet"], "10.0.0.0/8"),
        ("inet_merge", &["cidr", "cidr"], "10.0.0.0/8"),
        ("inet_merge", &["cidr", "macaddr"], "42883 function inet_merge(cidr, macaddr) does not exist"),
        ("inet_merge", &["cidr", "macaddr8"], "42883 function inet_merge(cidr, macaddr8) does not exist"),
        ("inet_merge", &["macaddr", "inet"], "42883 function inet_merge(macaddr, inet) does not exist"),
        ("inet_merge", &["macaddr", "cidr"], "42883 function inet_merge(macaddr, cidr) does not exist"),
        ("inet_merge", &["macaddr", "macaddr"], "42883 function inet_merge(macaddr, macaddr) does not exist"),
        ("inet_merge", &["macaddr", "macaddr8"], "42883 function inet_merge(macaddr, macaddr8) does not exist"),
        ("inet_merge", &["macaddr8", "inet"], "42883 function inet_merge(macaddr8, inet) does not exist"),
        ("inet_merge", &["macaddr8", "cidr"], "42883 function inet_merge(macaddr8, cidr) does not exist"),
        ("inet_merge", &["macaddr8", "macaddr"], "42883 function inet_merge(macaddr8, macaddr) does not exist"),
        ("inet_merge", &["macaddr8", "macaddr8"], "42883 function inet_merge(macaddr8, macaddr8) does not exist"),
    ];

    const CONVERSIONS: &[(&str, &[&str], &str)] = &[
        ("to_inet", &["inet"], "10.1.2.3/8"),
        ("fn_inet", &["inet"], "10.1.2.3/8"),
        ("to_inet", &["cidr"], "10.0.0.0/8"),
        ("fn_inet", &["cidr"], "10.0.0.0/8"),
        ("to_inet", &["macaddr"], "42846 cannot cast type macaddr to inet"),
        ("fn_inet", &["macaddr"], "42883 function inet(macaddr) does not exist"),
        ("to_inet", &["macaddr8"], "42846 cannot cast type macaddr8 to inet"),
        ("fn_inet", &["macaddr8"], "42883 function inet(macaddr8) does not exist"),
        ("to_cidr", &["inet"], "10.0.0.0/8"),
        ("fn_cidr", &["inet"], "10.0.0.0/8"),
        ("to_cidr", &["cidr"], "10.0.0.0/8"),
        ("fn_cidr", &["cidr"], "10.0.0.0/8"),
        ("to_cidr", &["macaddr"], "42846 cannot cast type macaddr to cidr"),
        ("fn_cidr", &["macaddr"], "42883 function cidr(macaddr) does not exist"),
        ("to_cidr", &["macaddr8"], "42846 cannot cast type macaddr8 to cidr"),
        ("fn_cidr", &["macaddr8"], "42883 function cidr(macaddr8) does not exist"),
        ("to_macaddr", &["inet"], "42846 cannot cast type inet to macaddr"),
        ("fn_macaddr", &["inet"], "42883 function macaddr(inet) does not exist"),
        ("to_macaddr", &["cidr"], "42846 cannot cast type cidr to macaddr"),
        ("fn_macaddr", &["cidr"], "42883 function macaddr(cidr) does not exist"),
        ("to_macaddr", &["macaddr"], "08:00:2b:01:02:03"),
        ("fn_macaddr", &["macaddr"], "08:00:2b:01:02:03"),
        ("to_macaddr", &["macaddr8"], "08:00:2b:01:02:03"),
        ("fn_macaddr", &["macaddr8"], "08:00:2b:01:02:03"),
        ("to_macaddr8", &["inet"], "42846 cannot cast type inet to macaddr8"),
        ("fn_macaddr8", &["inet"], "42883 function macaddr8(inet) does not exist"),
        ("to_macaddr8", &["cidr"], "42846 cannot cast type cidr to macaddr8"),
        ("fn_macaddr8", &["cidr"], "42883 function macaddr8(cidr) does not exist"),
        ("to_macaddr8", &["macaddr"], "08:00:2b:ff:fe:01:02:03"),
        ("fn_macaddr8", &["macaddr"], "08:00:2b:ff:fe:01:02:03"),
        ("to_macaddr8", &["macaddr8"], "08:00:2b:ff:fe:01:02:03"),
        ("fn_macaddr8", &["macaddr8"], "08:00:2b:ff:fe:01:02:03"),
        ("to_macaddr", &["macaddr8_nofffe"], "22003 macaddr8 data out of range to convert to macaddr"),
        ("fn_macaddr", &["macaddr8_nofffe"], "22003 macaddr8 data out of range to convert to macaddr"),
    ];
}
