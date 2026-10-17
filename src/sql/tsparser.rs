//! The default text search parser (`pg_catalog."default"`): Postgres's wparser_def.c, state table for state table. A
//! text is cut into tokens of 23 types (words, numbers, hosts, emails, URLs, paths, tags, blanks, ...) by a state
//! machine that may push its position and come back to it when a longer reading fails, so what a token is can
//! depend on what follows it far on (`8e296abc` is `8e296` then `abc`, `a.b/c` a file).

/// Where a state's actions end the tokens, and what they do next (A_* in wparser_def.c).
const A_NEXT: u16 = 0x0000;
const A_BINGO: u16 = 0x0001;
const A_POP: u16 = 0x0002;
const A_PUSH: u16 = 0x0004;
const A_RERUN: u16 = 0x0008;
const A_CLEAR: u16 = 0x0010;
const A_MERGE: u16 = 0x0020;
const A_CLRALL: u16 = 0x0040;

/// The character tests of an action (the p_is* functions); `Always` is the table's last item, which takes what the
/// rest didn't.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum T {
    Eof,
    EqC,
    Ignore,
    AscLet,
    Alpha,
    Digit,
    Alnum,
    NotAlnum,
    Space,
    XDigit,
    Special,
    StopHost,
    IsHost,
    IsUrlPath,
    UrlChar,
    Always,
}

/// The handlers some actions call (SpecialTags and the others).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Sp {
    None,
    Tags,
    FUrl,
    Hyphen,
    VerVersion,
}

pub(super) struct Item {
    test: T,
    c: char,
    flags: u16,
    to: St,
    ty: u8,
    special: Sp,
}

const HOST: u8 = 6;
const URLPATH: u8 = 18;

// Generated from PostgreSQL 16.14 src/backend/tsearch/wparser_def.c (the default parser's state tables).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(super) enum St {
    Base,
    InNumWord,
    InAsciiWord,
    InWord,
    InUnsignedInt,
    InSignedIntFirst,
    InSignedInt,
    InSpace,
    InUDecimalFirst,
    InUDecimal,
    InDecimalFirst,
    InDecimal,
    InVerVersion,
    InSVerVersion,
    InVersionFirst,
    InVersion,
    InMantissaFirst,
    InMantissaSign,
    InMantissa,
    InXMLEntityFirst,
    InXMLEntity,
    InXMLEntityNumFirst,
    InXMLEntityNum,
    InXMLEntityHexNumFirst,
    InXMLEntityHexNum,
    InXMLEntityEnd,
    InTagFirst,
    InXMLBegin,
    InTagCloseFirst,
    InTagName,
    InTagBeginEnd,
    InTag,
    InTagEscapeK,
    InTagEscapeKK,
    InTagBackSleshed,
    InTagEnd,
    InCommentFirst,
    InCommentLast,
    InComment,
    InCloseCommentFirst,
    InCloseCommentLast,
    InCommentEnd,
    InHostFirstDomain,
    InHostDomainSecond,
    InHostDomain,
    InPortFirst,
    InPort,
    InHostFirstAN,
    InHost,
    InEmail,
    InFileFirst,
    InFileTwiddle,
    InPathFirst,
    InPathFirstFirst,
    InPathSecond,
    InFile,
    InFileNext,
    InURLPathFirst,
    InURLPathStart,
    InURLPath,
    InFURL,
    InProtocolFirst,
    InProtocolSecond,
    InProtocolEnd,
    InHyphenAsciiWordFirst,
    InHyphenAsciiWord,
    InHyphenWordFirst,
    InHyphenWord,
    InHyphenNumWordFirst,
    InHyphenNumWord,
    InHyphenDigitLookahead,
    InParseHyphen,
    InParseHyphenHyphen,
    InHyphenWordPart,
    InHyphenAsciiWordPart,
    InHyphenNumWordPart,
    InHyphenUnsignedInt,
    Null,
}

const A_BASE: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '<', flags: A_PUSH, to: St::InTagFirst, ty: 0, special: Sp::None },
    Item { test: T::Ignore, c: '\0', flags: A_NEXT, to: St::InSpace, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InAsciiWord, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InWord, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InUnsignedInt, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InSignedIntFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '+', flags: A_PUSH, to: St::InSignedIntFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '&', flags: A_PUSH, to: St::InXMLEntityFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '~', flags: A_PUSH, to: St::InFileTwiddle, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_PUSH, to: St::InFileFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InPathFirstFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_NEXT, to: St::InSpace, ty: 0, special: Sp::None },
];

const A_INNUMWORD: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 3, special: Sp::None },
    Item { test: T::Alnum, c: '\0', flags: A_NEXT, to: St::InNumWord, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InNumWord, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '@', flags: A_PUSH, to: St::InEmail, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_PUSH, to: St::InFileFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InFileNext, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHyphenNumWordFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 3, special: Sp::None },
];

const A_INASCIIWORD: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 1, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InHostFirstDomain, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InFileNext, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHyphenAsciiWordFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '@', flags: A_PUSH, to: St::InEmail, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ':', flags: A_PUSH, to: St::InProtocolFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_PUSH, to: St::InFileFirst, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_PUSH, to: St::InHost, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InNumWord, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InWord, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InWord, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 1, special: Sp::None },
];

const A_INWORD: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 2, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InNumWord, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHyphenWordFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 2, special: Sp::None },
];

const A_INUNSIGNEDINT: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 22, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InHostFirstDomain, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InUDecimalFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'e', flags: A_PUSH, to: St::InMantissaFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'E', flags: A_PUSH, to: St::InMantissaFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '@', flags: A_PUSH, to: St::InEmail, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_PUSH, to: St::InHost, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InNumWord, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InNumWord, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_PUSH, to: St::InFileFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 22, special: Sp::None },
];

const A_INSIGNEDINTFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT | A_CLEAR, to: St::InSignedInt, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INSIGNEDINT: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 21, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InDecimalFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'e', flags: A_PUSH, to: St::InMantissaFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'E', flags: A_PUSH, to: St::InMantissaFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 21, special: Sp::None },
];

const A_INSPACE: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 12, special: Sp::None },
    Item { test: T::EqC, c: '<', flags: A_BINGO, to: St::Base, ty: 12, special: Sp::None },
    Item { test: T::Ignore, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_BINGO, to: St::Base, ty: 12, special: Sp::None },
    Item { test: T::EqC, c: '+', flags: A_BINGO, to: St::Base, ty: 12, special: Sp::None },
    Item { test: T::EqC, c: '&', flags: A_BINGO, to: St::Base, ty: 12, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_BINGO, to: St::Base, ty: 12, special: Sp::None },
    Item { test: T::NotAlnum, c: '\0', flags: A_NEXT, to: St::InSpace, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 12, special: Sp::None },
];

const A_INUDECIMALFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_CLEAR, to: St::InUDecimal, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INUDECIMAL: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 20, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InUDecimal, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InVersionFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'e', flags: A_PUSH, to: St::InMantissaFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'E', flags: A_PUSH, to: St::InMantissaFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 20, special: Sp::None },
];

const A_INDECIMALFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_CLEAR, to: St::InDecimal, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INDECIMAL: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 20, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InDecimal, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InVerVersion, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'e', flags: A_PUSH, to: St::InMantissaFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'E', flags: A_PUSH, to: St::InMantissaFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 20, special: Sp::None },
];

const A_INVERVERSION: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_RERUN, to: St::InSVerVersion, ty: 0, special: Sp::VerVersion },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INSVERVERSION: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_BINGO | A_CLRALL, to: St::InUnsignedInt, ty: 12, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
];

const A_INVERSIONFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_CLEAR, to: St::InVersion, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INVERSION: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 8, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InVersion, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InVersionFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 8, special: Sp::None },
];

const A_INMANTISSAFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_CLEAR, to: St::InMantissa, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '+', flags: A_NEXT, to: St::InMantissaSign, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::InMantissaSign, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INMANTISSASIGN: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_CLEAR, to: St::InMantissa, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INMANTISSA: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 7, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InMantissa, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 7, special: Sp::None },
];

const A_INXMLENTITYFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '#', flags: A_NEXT, to: St::InXMLEntityNumFirst, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InXMLEntity, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ':', flags: A_NEXT, to: St::InXMLEntity, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_NEXT, to: St::InXMLEntity, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INXMLENTITY: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Alnum, c: '\0', flags: A_NEXT, to: St::InXMLEntity, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ':', flags: A_NEXT, to: St::InXMLEntity, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_NEXT, to: St::InXMLEntity, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_NEXT, to: St::InXMLEntity, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::InXMLEntity, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ';', flags: A_NEXT, to: St::InXMLEntityEnd, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INXMLENTITYNUMFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'x', flags: A_NEXT, to: St::InXMLEntityHexNumFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'X', flags: A_NEXT, to: St::InXMLEntityHexNumFirst, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InXMLEntityNum, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INXMLENTITYNUM: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InXMLEntityNum, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ';', flags: A_NEXT, to: St::InXMLEntityEnd, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INXMLENTITYHEXNUMFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::XDigit, c: '\0', flags: A_NEXT, to: St::InXMLEntityHexNum, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INXMLENTITYHEXNUM: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::XDigit, c: '\0', flags: A_NEXT, to: St::InXMLEntityHexNum, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ';', flags: A_NEXT, to: St::InXMLEntityEnd, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INXMLENTITYEND: &[Item] =
    &[Item { test: T::Always, c: '\0', flags: A_BINGO | A_CLEAR, to: St::Base, ty: 23, special: Sp::None }];

const A_INTAGFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_PUSH, to: St::InTagCloseFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '!', flags: A_PUSH, to: St::InCommentFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '?', flags: A_PUSH, to: St::InXMLBegin, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_PUSH, to: St::InTagName, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ':', flags: A_PUSH, to: St::InTagName, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_PUSH, to: St::InTagName, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INXMLBEGIN: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'x', flags: A_NEXT, to: St::InTag, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INTAGCLOSEFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InTagName, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INTAGNAME: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_NEXT, to: St::InTagBeginEnd, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '>', flags: A_NEXT, to: St::InTagEnd, ty: 0, special: Sp::Tags },
    Item { test: T::Space, c: '\0', flags: A_NEXT, to: St::InTag, ty: 0, special: Sp::Tags },
    Item { test: T::Alnum, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ':', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INTAGBEGINEND: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '>', flags: A_NEXT, to: St::InTagEnd, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INTAG: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '>', flags: A_NEXT, to: St::InTagEnd, ty: 0, special: Sp::Tags },
    Item { test: T::EqC, c: '\'', flags: A_NEXT, to: St::InTagEscapeK, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '"', flags: A_NEXT, to: St::InTagEscapeKK, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '=', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '#', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ':', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '&', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '?', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '%', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '~', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Space, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::Tags },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INTAGESCAPEK: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '\\', flags: A_PUSH, to: St::InTagBackSleshed, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '\'', flags: A_NEXT, to: St::InTag, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_NEXT, to: St::InTagEscapeK, ty: 0, special: Sp::None },
];

const A_INTAGESCAPEKK: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '\\', flags: A_PUSH, to: St::InTagBackSleshed, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '"', flags: A_NEXT, to: St::InTag, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_NEXT, to: St::InTagEscapeKK, ty: 0, special: Sp::None },
];

const A_INTAGBACKSLESHED: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_MERGE, to: St::Null, ty: 0, special: Sp::None },
];

const A_INTAGEND: &[Item] =
    &[Item { test: T::Always, c: '\0', flags: A_BINGO | A_CLRALL, to: St::Base, ty: 13, special: Sp::None }];

const A_INCOMMENTFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::InCommentLast, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'D', flags: A_NEXT, to: St::InTag, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: 'd', flags: A_NEXT, to: St::InTag, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INCOMMENTLAST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::InComment, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INCOMMENT: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::InCloseCommentFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
];

const A_INCLOSECOMMENTFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::InCloseCommentLast, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_NEXT, to: St::InComment, ty: 0, special: Sp::None },
];

const A_INCLOSECOMMENTLAST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '>', flags: A_NEXT, to: St::InCommentEnd, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_NEXT, to: St::InComment, ty: 0, special: Sp::None },
];

const A_INCOMMENTEND: &[Item] =
    &[Item { test: T::Always, c: '\0', flags: A_BINGO | A_CLRALL, to: St::Base, ty: 13, special: Sp::None }];

const A_INHOSTFIRSTDOMAIN: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InHostDomainSecond, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHost, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INHOSTDOMAINSECOND: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InHostDomain, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_PUSH, to: St::InHost, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InHostFirstDomain, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '@', flags: A_PUSH, to: St::InEmail, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INHOSTDOMAIN: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO | A_CLRALL, to: St::Base, ty: 6, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InHostDomain, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_PUSH, to: St::InHost, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: ':', flags: A_PUSH, to: St::InPortFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InHostFirstDomain, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '@', flags: A_PUSH, to: St::InEmail, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::StopHost, c: '\0', flags: A_BINGO | A_CLRALL, to: St::InURLPathStart, ty: 6, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_PUSH, to: St::InFURL, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO | A_CLRALL, to: St::Base, ty: 6, special: Sp::None },
];

const A_INPORTFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InPort, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INPORT: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO | A_CLRALL, to: St::Base, ty: 6, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InPort, ty: 0, special: Sp::None },
    Item { test: T::StopHost, c: '\0', flags: A_BINGO | A_CLRALL, to: St::InURLPathStart, ty: 6, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_PUSH, to: St::InFURL, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO | A_CLRALL, to: St::Base, ty: 6, special: Sp::None },
];

const A_INHOSTFIRSTAN: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHost, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InHost, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INHOST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHost, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InHost, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '@', flags: A_PUSH, to: St::InEmail, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InHostFirstDomain, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_PUSH, to: St::InHostFirstAN, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INEMAIL: &[Item] = &[
    Item { test: T::StopHost, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::IsHost, c: '\0', flags: A_BINGO | A_CLRALL, to: St::Base, ty: 4, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INFILEFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_NEXT, to: St::InPathFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '~', flags: A_PUSH, to: St::InFileTwiddle, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INFILETWIDDLE: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_NEXT, to: St::InFileFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INPATHFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_NEXT, to: St::InPathSecond, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_NEXT, to: St::InFileFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INPATHFIRSTFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_NEXT, to: St::InPathSecond, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_NEXT, to: St::InFileFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INPATHSECOND: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO | A_CLEAR, to: St::Base, ty: 19, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_NEXT | A_PUSH, to: St::InFileFirst, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_BINGO | A_CLEAR, to: St::Base, ty: 19, special: Sp::None },
    Item { test: T::Space, c: '\0', flags: A_BINGO | A_CLEAR, to: St::Base, ty: 19, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INFILE: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 19, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '.', flags: A_PUSH, to: St::InFileNext, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_NEXT, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_PUSH, to: St::InFileFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 19, special: Sp::None },
];

const A_INFILENEXT: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_CLEAR, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_CLEAR, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '_', flags: A_CLEAR, to: St::InFile, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INURLPATHFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::UrlChar, c: '\0', flags: A_NEXT, to: St::InURLPath, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INURLPATHSTART: &[Item] =
    &[Item { test: T::Always, c: '\0', flags: A_NEXT, to: St::InURLPath, ty: 0, special: Sp::None }];

const A_INURLPATH: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 18, special: Sp::None },
    Item { test: T::UrlChar, c: '\0', flags: A_NEXT, to: St::InURLPath, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::Base, ty: 18, special: Sp::None },
];

const A_INFURL: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::IsUrlPath, c: '\0', flags: A_BINGO | A_CLRALL, to: St::Base, ty: 5, special: Sp::FUrl },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INPROTOCOLFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_NEXT, to: St::InProtocolSecond, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INPROTOCOLSECOND: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '/', flags: A_NEXT, to: St::InProtocolEnd, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INPROTOCOLEND: &[Item] =
    &[Item { test: T::Always, c: '\0', flags: A_BINGO | A_CLRALL, to: St::Base, ty: 14, special: Sp::None }];

const A_INHYPHENASCIIWORDFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InHyphenAsciiWord, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InHyphenWord, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHyphenDigitLookahead, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INHYPHENASCIIWORD: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO | A_CLRALL, to: St::InParseHyphen, ty: 16, special: Sp::Hyphen },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InHyphenAsciiWord, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InHyphenWord, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InHyphenWord, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHyphenNumWord, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHyphenAsciiWordFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO | A_CLRALL, to: St::InParseHyphen, ty: 16, special: Sp::Hyphen },
];

const A_INHYPHENWORDFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InHyphenWord, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHyphenDigitLookahead, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INHYPHENWORD: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO | A_CLRALL, to: St::InParseHyphen, ty: 17, special: Sp::Hyphen },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InHyphenWord, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InHyphenWord, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHyphenNumWord, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHyphenWordFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO | A_CLRALL, to: St::InParseHyphen, ty: 17, special: Sp::Hyphen },
];

const A_INHYPHENNUMWORDFIRST: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InHyphenNumWord, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHyphenDigitLookahead, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INHYPHENNUMWORD: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO | A_CLRALL, to: St::InParseHyphen, ty: 15, special: Sp::Hyphen },
    Item { test: T::Alnum, c: '\0', flags: A_NEXT, to: St::InHyphenNumWord, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InHyphenNumWord, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InHyphenNumWordFirst, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO | A_CLRALL, to: St::InParseHyphen, ty: 15, special: Sp::Hyphen },
];

const A_INHYPHENDIGITLOOKAHEAD: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHyphenDigitLookahead, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InHyphenNumWord, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InHyphenNumWord, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INPARSEHYPHEN: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_RERUN, to: St::Base, ty: 0, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InHyphenAsciiWordPart, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InHyphenWordPart, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_PUSH, to: St::InHyphenUnsignedInt, ty: 0, special: Sp::None },
    Item { test: T::EqC, c: '-', flags: A_PUSH, to: St::InParseHyphenHyphen, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_RERUN, to: St::Base, ty: 0, special: Sp::None },
];

const A_INPARSEHYPHENHYPHEN: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Alnum, c: '\0', flags: A_BINGO | A_CLEAR, to: St::InParseHyphen, ty: 12, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_BINGO | A_CLEAR, to: St::InParseHyphen, ty: 12, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

const A_INHYPHENWORDPART: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 10, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InHyphenWordPart, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InHyphenWordPart, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHyphenNumWordPart, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::InParseHyphen, ty: 10, special: Sp::None },
];

const A_INHYPHENASCIIWORDPART: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 11, special: Sp::None },
    Item { test: T::AscLet, c: '\0', flags: A_NEXT, to: St::InHyphenAsciiWordPart, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_NEXT, to: St::InHyphenWordPart, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InHyphenWordPart, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::InHyphenNumWordPart, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::InParseHyphen, ty: 11, special: Sp::None },
];

const A_INHYPHENNUMWORDPART: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_BINGO, to: St::Base, ty: 9, special: Sp::None },
    Item { test: T::Alnum, c: '\0', flags: A_NEXT, to: St::InHyphenNumWordPart, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_NEXT, to: St::InHyphenNumWordPart, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_BINGO, to: St::InParseHyphen, ty: 9, special: Sp::None },
];

const A_INHYPHENUNSIGNEDINT: &[Item] = &[
    Item { test: T::Eof, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Digit, c: '\0', flags: A_NEXT, to: St::Null, ty: 0, special: Sp::None },
    Item { test: T::Alpha, c: '\0', flags: A_CLEAR, to: St::InHyphenNumWordPart, ty: 0, special: Sp::None },
    Item { test: T::Special, c: '\0', flags: A_CLEAR, to: St::InHyphenNumWordPart, ty: 0, special: Sp::None },
    Item { test: T::Always, c: '\0', flags: A_POP, to: St::Null, ty: 0, special: Sp::None },
];

pub(super) fn actions(s: St) -> &'static [Item] {
    match s {
        St::Base => A_BASE,
        St::InNumWord => A_INNUMWORD,
        St::InAsciiWord => A_INASCIIWORD,
        St::InWord => A_INWORD,
        St::InUnsignedInt => A_INUNSIGNEDINT,
        St::InSignedIntFirst => A_INSIGNEDINTFIRST,
        St::InSignedInt => A_INSIGNEDINT,
        St::InSpace => A_INSPACE,
        St::InUDecimalFirst => A_INUDECIMALFIRST,
        St::InUDecimal => A_INUDECIMAL,
        St::InDecimalFirst => A_INDECIMALFIRST,
        St::InDecimal => A_INDECIMAL,
        St::InVerVersion => A_INVERVERSION,
        St::InSVerVersion => A_INSVERVERSION,
        St::InVersionFirst => A_INVERSIONFIRST,
        St::InVersion => A_INVERSION,
        St::InMantissaFirst => A_INMANTISSAFIRST,
        St::InMantissaSign => A_INMANTISSASIGN,
        St::InMantissa => A_INMANTISSA,
        St::InXMLEntityFirst => A_INXMLENTITYFIRST,
        St::InXMLEntity => A_INXMLENTITY,
        St::InXMLEntityNumFirst => A_INXMLENTITYNUMFIRST,
        St::InXMLEntityNum => A_INXMLENTITYNUM,
        St::InXMLEntityHexNumFirst => A_INXMLENTITYHEXNUMFIRST,
        St::InXMLEntityHexNum => A_INXMLENTITYHEXNUM,
        St::InXMLEntityEnd => A_INXMLENTITYEND,
        St::InTagFirst => A_INTAGFIRST,
        St::InXMLBegin => A_INXMLBEGIN,
        St::InTagCloseFirst => A_INTAGCLOSEFIRST,
        St::InTagName => A_INTAGNAME,
        St::InTagBeginEnd => A_INTAGBEGINEND,
        St::InTag => A_INTAG,
        St::InTagEscapeK => A_INTAGESCAPEK,
        St::InTagEscapeKK => A_INTAGESCAPEKK,
        St::InTagBackSleshed => A_INTAGBACKSLESHED,
        St::InTagEnd => A_INTAGEND,
        St::InCommentFirst => A_INCOMMENTFIRST,
        St::InCommentLast => A_INCOMMENTLAST,
        St::InComment => A_INCOMMENT,
        St::InCloseCommentFirst => A_INCLOSECOMMENTFIRST,
        St::InCloseCommentLast => A_INCLOSECOMMENTLAST,
        St::InCommentEnd => A_INCOMMENTEND,
        St::InHostFirstDomain => A_INHOSTFIRSTDOMAIN,
        St::InHostDomainSecond => A_INHOSTDOMAINSECOND,
        St::InHostDomain => A_INHOSTDOMAIN,
        St::InPortFirst => A_INPORTFIRST,
        St::InPort => A_INPORT,
        St::InHostFirstAN => A_INHOSTFIRSTAN,
        St::InHost => A_INHOST,
        St::InEmail => A_INEMAIL,
        St::InFileFirst => A_INFILEFIRST,
        St::InFileTwiddle => A_INFILETWIDDLE,
        St::InPathFirst => A_INPATHFIRST,
        St::InPathFirstFirst => A_INPATHFIRSTFIRST,
        St::InPathSecond => A_INPATHSECOND,
        St::InFile => A_INFILE,
        St::InFileNext => A_INFILENEXT,
        St::InURLPathFirst => A_INURLPATHFIRST,
        St::InURLPathStart => A_INURLPATHSTART,
        St::InURLPath => A_INURLPATH,
        St::InFURL => A_INFURL,
        St::InProtocolFirst => A_INPROTOCOLFIRST,
        St::InProtocolSecond => A_INPROTOCOLSECOND,
        St::InProtocolEnd => A_INPROTOCOLEND,
        St::InHyphenAsciiWordFirst => A_INHYPHENASCIIWORDFIRST,
        St::InHyphenAsciiWord => A_INHYPHENASCIIWORD,
        St::InHyphenWordFirst => A_INHYPHENWORDFIRST,
        St::InHyphenWord => A_INHYPHENWORD,
        St::InHyphenNumWordFirst => A_INHYPHENNUMWORDFIRST,
        St::InHyphenNumWord => A_INHYPHENNUMWORD,
        St::InHyphenDigitLookahead => A_INHYPHENDIGITLOOKAHEAD,
        St::InParseHyphen => A_INPARSEHYPHEN,
        St::InParseHyphenHyphen => A_INPARSEHYPHENHYPHEN,
        St::InHyphenWordPart => A_INHYPHENWORDPART,
        St::InHyphenAsciiWordPart => A_INHYPHENASCIIWORDPART,
        St::InHyphenNumWordPart => A_INHYPHENNUMWORDPART,
        St::InHyphenUnsignedInt => A_INHYPHENUNSIGNEDINT,
        St::Null => &[],
    }
}

/// Spacing combining marks that are not letters but do not break a word (`p_isspecial`), in order.
pub(super) const STRANGE: [u32; 228] = [
    0x0903, 0x093E, 0x093F, 0x0940, 0x0949, 0x094A, 0x094B, 0x094C, 0x0982, 0x0983, 0x09BE, 0x09BF, 0x09C0, 0x09C7,
    0x09C8, 0x09CB, 0x09CC, 0x09D7, 0x0A03, 0x0A3E, 0x0A3F, 0x0A40, 0x0A83, 0x0ABE, 0x0ABF, 0x0AC0, 0x0AC9, 0x0ACB,
    0x0ACC, 0x0B02, 0x0B03, 0x0B3E, 0x0B40, 0x0B47, 0x0B48, 0x0B4B, 0x0B4C, 0x0B57, 0x0BBE, 0x0BBF, 0x0BC1, 0x0BC2,
    0x0BC6, 0x0BC7, 0x0BC8, 0x0BCA, 0x0BCB, 0x0BCC, 0x0BD7, 0x0C01, 0x0C02, 0x0C03, 0x0C41, 0x0C42, 0x0C43, 0x0C44,
    0x0C82, 0x0C83, 0x0CBE, 0x0CC0, 0x0CC1, 0x0CC2, 0x0CC3, 0x0CC4, 0x0CC7, 0x0CC8, 0x0CCA, 0x0CCB, 0x0CD5, 0x0CD6,
    0x0D02, 0x0D03, 0x0D3E, 0x0D3F, 0x0D40, 0x0D46, 0x0D47, 0x0D48, 0x0D4A, 0x0D4B, 0x0D4C, 0x0D57, 0x0D82, 0x0D83,
    0x0DCF, 0x0DD0, 0x0DD1, 0x0DD8, 0x0DD9, 0x0DDA, 0x0DDB, 0x0DDC, 0x0DDD, 0x0DDE, 0x0DDF, 0x0DF2, 0x0DF3, 0x0F3E,
    0x0F3F, 0x0F7F, 0x102B, 0x102C, 0x1031, 0x1038, 0x103B, 0x103C, 0x1056, 0x1057, 0x1062, 0x1063, 0x1064, 0x1067,
    0x1068, 0x1069, 0x106A, 0x106B, 0x106C, 0x106D, 0x1083, 0x1084, 0x1087, 0x1088, 0x1089, 0x108A, 0x108B, 0x108C,
    0x108F, 0x17B6, 0x17BE, 0x17BF, 0x17C0, 0x17C1, 0x17C2, 0x17C3, 0x17C4, 0x17C5, 0x17C7, 0x17C8, 0x1923, 0x1924,
    0x1925, 0x1926, 0x1929, 0x192A, 0x192B, 0x1930, 0x1931, 0x1933, 0x1934, 0x1935, 0x1936, 0x1937, 0x1938, 0x19B0,
    0x19B1, 0x19B2, 0x19B3, 0x19B4, 0x19B5, 0x19B6, 0x19B7, 0x19B8, 0x19B9, 0x19BA, 0x19BB, 0x19BC, 0x19BD, 0x19BE,
    0x19BF, 0x19C0, 0x19C8, 0x19C9, 0x1A19, 0x1A1A, 0x1A1B, 0x1B04, 0x1B35, 0x1B3B, 0x1B3D, 0x1B3E, 0x1B3F, 0x1B40,
    0x1B41, 0x1B43, 0x1B44, 0x1B82, 0x1BA1, 0x1BA6, 0x1BA7, 0x1BAA, 0x1C24, 0x1C25, 0x1C26, 0x1C27, 0x1C28, 0x1C29,
    0x1C2A, 0x1C2B, 0x1C34, 0x1C35, 0xA823, 0xA824, 0xA827, 0xA880, 0xA881, 0xA8B4, 0xA8B5, 0xA8B6, 0xA8B7, 0xA8B8,
    0xA8B9, 0xA8BA, 0xA8BB, 0xA8BC, 0xA8BD, 0xA8BE, 0xA8BF, 0xA8C0, 0xA8C1, 0xA8C2, 0xA8C3, 0xA952, 0xA953, 0xAA2F,
    0xAA30, 0xAA33, 0xAA34, 0xAA4D,
];

/// One place the parser can come back to: where it is, how much of the token it has, and the state it was in.
#[derive(Clone, Copy)]
struct Position {
    pos: usize,
    /// the characters of the token so far
    len: usize,
    state: St,
    /// the item that pushed this position (a pop picks up at the one after it)
    pushed_at: Option<usize>,
}

struct Parser<'a> {
    s: &'a [char],
    stack: Vec<Position>,
    /// inside a `<script>` or `<style>`: everything is a blank
    ignore: bool,
    /// the next host test ends the host (a host is wanted, not a longer token)
    wanthost: bool,
    /// the character the current item compares with
    c: char,
    /// the token just found: where it starts, how long it is and its type
    start: usize,
    len: usize,
    ty: u8,
}

/// Whether a character is a letter, as iswalpha has it in a UTF-8 locale (the digits of other scripts count: a
/// glibc letter class holds them).
fn is_alpha(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic();
    }
    c.is_alphabetic()
        || matches!(c as u32,
            0x0660..=0x0669 | 0x06F0..=0x06F9 | 0x07C0..=0x07C9 | 0x0966..=0x096F | 0x09E6..=0x09EF
            | 0x0A66..=0x0A6F | 0x0AE6..=0x0AEF | 0x0B66..=0x0B6F | 0x0BE6..=0x0BEF | 0x0C66..=0x0C6F
            | 0x0CE6..=0x0CEF | 0x0D66..=0x0D6F | 0x0E50..=0x0E59 | 0x0ED0..=0x0ED9 | 0x0F20..=0x0F29
            | 0x1040..=0x1049 | 0x17E0..=0x17E9 | 0x1810..=0x1819 | 0xFF10..=0xFF19)
}

/// A character with no display width (pg_dsplen is 0), which doesn't break a word.
fn zero_width(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7
        | 0x0600..=0x0603 | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06E4 | 0x06E7..=0x06E8
        | 0x06EA..=0x06ED | 0x070F | 0x0711 | 0x0730..=0x074A | 0x07A6..=0x07B0 | 0x07EB..=0x07F3
        | 0x0901..=0x0902 | 0x093C | 0x0941..=0x0948 | 0x094D | 0x0951..=0x0954 | 0x0962..=0x0963 | 0x0981
        | 0x09BC | 0x09C1..=0x09C4 | 0x09CD | 0x09E2..=0x09E3 | 0x0A01..=0x0A02 | 0x0A3C | 0x0A41..=0x0A42
        | 0x0A47..=0x0A48 | 0x0A4B..=0x0A4D | 0x0A70..=0x0A71 | 0x0A81..=0x0A82 | 0x0ABC | 0x0AC1..=0x0AC5
        | 0x0AC7..=0x0AC8 | 0x0ACD | 0x0AE2..=0x0AE3 | 0x0B01 | 0x0B3C | 0x0B3F | 0x0B41..=0x0B43 | 0x0B4D
        | 0x0B56 | 0x0B82 | 0x0BC0 | 0x0BCD | 0x0C3E..=0x0C40 | 0x0C46..=0x0C48 | 0x0C4A..=0x0C4D
        | 0x0C55..=0x0C56 | 0x0CBC | 0x0CBF | 0x0CC6 | 0x0CCC..=0x0CCD | 0x0CE2..=0x0CE3 | 0x0D41..=0x0D43
        | 0x0D4D | 0x0DCA | 0x0DD2..=0x0DD4 | 0x0DD6 | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E | 0x0EB1
        | 0x0EB4..=0x0EB9 | 0x0EBB..=0x0EBC | 0x0EC8..=0x0ECD | 0x0F18..=0x0F19 | 0x0F35 | 0x0F37 | 0x0F39
        | 0x0F71..=0x0F7E | 0x0F80..=0x0F84 | 0x0F86..=0x0F87 | 0x0F90..=0x0F97 | 0x0F99..=0x0FBC | 0x0FC6
        | 0x102D..=0x1030 | 0x1032 | 0x1036..=0x1037 | 0x1039 | 0x1058..=0x1059 | 0x1160..=0x11FF
        | 0x135F | 0x1712..=0x1714 | 0x1732..=0x1734 | 0x1752..=0x1753 | 0x1772..=0x1773 | 0x17B4..=0x17B5
        | 0x17B7..=0x17BD | 0x17C6 | 0x17C9..=0x17D3 | 0x17DD | 0x180B..=0x180D | 0x18A9 | 0x1920..=0x1922
        | 0x1927..=0x1928 | 0x1932 | 0x1939..=0x193B | 0x1A17..=0x1A18 | 0x1B00..=0x1B03 | 0x1B34 | 0x1B36..=0x1B3A
        | 0x1B3C | 0x1B42 | 0x1B6B..=0x1B73 | 0x1DC0..=0x1DCA | 0x1DFE..=0x1DFF | 0x200B..=0x200F
        | 0x202A..=0x202E | 0x2060..=0x2063 | 0x206A..=0x206F | 0x20D0..=0x20EF | 0x302A..=0x302F
        | 0x3099..=0x309A | 0xA806 | 0xA80B | 0xA825..=0xA826 | 0xFB1E | 0xFE00..=0xFE0F | 0xFE20..=0xFE23
        | 0xFEFF | 0xFFF9..=0xFFFB)
}

impl<'a> Parser<'a> {
    fn new(s: &'a [char]) -> Parser<'a> {
        Parser {
            s,
            stack: vec![Position { pos: 0, len: 0, state: St::Base, pushed_at: None }],
            ignore: false,
            wanthost: false,
            c: '\0',
            start: 0,
            len: 0,
            ty: 0,
        }
    }

    fn cur(&self) -> &Position {
        self.stack.last().expect("parser position")
    }

    fn cur_mut(&mut self) -> &mut Position {
        self.stack.last_mut().expect("parser position")
    }

    /// The character at the current position (None at the end of the text).
    fn here(&self) -> Option<char> {
        self.s.get(self.cur().pos).copied()
    }

    /// A parser over the text from here on, as TParserCopyInit makes one (for `p_ishost` and `p_isURLPath`).
    fn copy(&self) -> Parser<'a> {
        Parser::new(&self.s[self.cur().pos.min(self.s.len())..])
    }

    /// The character test of an action (see the p_is* functions).
    fn test(&mut self, t: T) -> bool {
        let ch = self.here();
        match t {
            T::Eof => ch.is_none(),
            T::EqC => ch == Some(self.c),
            T::Ignore => self.ignore,
            T::AscLet => ch.is_some_and(|c| c.is_ascii_alphabetic()),
            T::Alpha => ch.is_some_and(is_alpha),
            T::Digit => ch.is_some_and(|c| c.is_ascii_digit()),
            T::Alnum => ch.is_some_and(|c| is_alpha(c) || c.is_ascii_digit()),
            T::NotAlnum => !ch.is_some_and(|c| is_alpha(c) || c.is_ascii_digit()),
            T::Space => {
                ch.is_some_and(|c| if c.is_ascii() { matches!(c, ' ' | '\t'..='\r') } else { c.is_whitespace() })
            }
            T::XDigit => ch.is_some_and(|c| c.is_ascii_hexdigit()),
            T::Special => {
                ch.is_some_and(|c| !c.is_ascii() && (zero_width(c) || STRANGE.binary_search(&(c as u32)).is_ok()))
            }
            T::StopHost => {
                let was = self.wanthost;
                self.wanthost = false;
                was
            }
            T::IsHost => {
                let mut tmp = self.copy();
                tmp.wanthost = true;
                if tmp.get() && tmp.ty == HOST {
                    let p = self.cur_mut();
                    p.pos += tmp.len;
                    p.len += tmp.len;
                    true
                } else {
                    false
                }
            }
            T::IsUrlPath => {
                let mut tmp = self.copy();
                let top = Position { state: St::InURLPathFirst, ..*tmp.cur() };
                tmp.stack.push(top);
                if tmp.get() && tmp.ty == URLPATH {
                    let p = self.cur_mut();
                    p.pos += tmp.len;
                    p.len += tmp.len;
                    true
                } else {
                    false
                }
            }
            // an ASCII character RFC 3986 allows in a URL
            T::UrlChar => ch.is_some_and(|c| {
                c.is_ascii()
                    && (c as u32) > 0x20
                    && (c as u32) < 0x7F
                    && !matches!(c, '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '|' | '}')
            }),
            T::Always => true,
        }
    }

    /// The special handlers (before the action's own effect).
    fn special(&mut self, sp: Sp) {
        match sp {
            Sp::None => {}
            // a tag that opens or closes a script or style ends or begins the text to ignore
            Sp::Tags => {
                let n = self.cur().len;
                let from = self.start;
                let head: String =
                    self.s[from..(from + n).min(self.s.len())].iter().collect::<String>().to_ascii_lowercase();
                match n {
                    8 if head == "</script" => self.ignore = false,
                    7 if head == "</style" => self.ignore = false,
                    7 if head == "<script" => self.ignore = true,
                    6 if head == "<style" => self.ignore = true,
                    _ => {}
                }
            }
            Sp::FUrl => {
                self.wanthost = true;
                let p = self.cur_mut();
                p.pos -= p.len;
            }
            Sp::Hyphen => {
                let p = self.cur_mut();
                p.pos -= p.len;
            }
            Sp::VerVersion => {
                let p = self.cur_mut();
                p.pos -= p.len;
                p.len = 0;
            }
        }
    }

    /// The next token (TParserGet): false at the end of the text; else its type is in `ty`, where it starts in
    /// `start` and its length in `len`.
    fn get(&mut self) -> bool {
        let n = self.s.len();
        if self.cur().pos >= n {
            return false;
        }
        self.start = self.cur().pos;
        self.cur_mut().pushed_at = None;
        let mut last = 0u16;
        let mut seen = false;
        while self.cur().pos <= n {
            let table = actions(self.cur().state);
            // after a pop, pick up at the test after the one that pushed
            let mut at = match self.cur().pushed_at {
                Some(p) => {
                    self.cur_mut().pushed_at = None;
                    p + 1
                }
                None => 0,
            };
            while table[at].test != T::Always {
                self.c = table[at].c;
                if self.test(table[at].test) {
                    break;
                }
                at += 1;
            }
            let item = &table[at];
            last = item.flags;
            seen = true;
            if item.special != Sp::None {
                self.special(item.special);
            }
            // BINGO: a token is found
            if item.flags & A_BINGO != 0 {
                self.len = self.cur().len;
                self.cur_mut().len = 0;
                self.ty = item.ty;
            }
            if item.flags & A_POP != 0 {
                self.stack.pop();
            } else if item.flags & A_PUSH != 0 {
                self.cur_mut().pushed_at = Some(at);
                let mut next = *self.cur();
                next.pushed_at = None;
                self.stack.push(next);
            } else if item.flags & A_CLEAR != 0 {
                let top = self.stack.len() - 1;
                if top > 0 {
                    self.stack.remove(top - 1);
                }
            } else if item.flags & A_CLRALL != 0 {
                let top = *self.cur();
                self.stack.clear();
                self.stack.push(top);
            } else if item.flags & A_MERGE != 0 {
                let top = self.stack.pop().expect("parser position");
                let prev = self.cur_mut();
                prev.pos = top.pos;
                prev.len = top.len;
            }
            if item.to != St::Null {
                self.cur_mut().state = item.to;
            }
            // check for go away
            if item.flags & A_BINGO != 0 || (self.cur().pos >= n && item.flags & A_RERUN == 0) {
                break;
            }
            if item.flags & (A_RERUN | A_POP) != 0 {
                continue;
            }
            // move forward
            if self.cur().pos < n {
                let p = self.cur_mut();
                p.pos += 1;
                p.len += 1;
            }
        }
        seen && last & A_BINGO != 0
    }
}

/// A text's tokens in order, with their types (1..=23, as `TOKEN_TYPES` lists them): blanks, tags and all.
pub(super) fn parse(s: &str) -> Vec<(u8, String)> {
    let c: Vec<char> = s.chars().collect();
    let mut p = Parser::new(&c);
    let mut out = vec![];
    while p.get() {
        out.push((p.ty, c[p.start..(p.start + p.len).min(c.len())].iter().collect()));
    }
    out
}
